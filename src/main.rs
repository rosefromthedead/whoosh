use std::{
    collections::HashMap,
    fs::{read_dir, read_to_string},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
use tracing::{debug, debug_span, error, field::Empty, info, info_span, trace, trace_span, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    config::{CompositeMode, Config},
    error::Error,
};

mod config;
mod error;
mod fan;

const RETRY_MS: u64 = 2000;

struct State {
    config: Config,
    sensor_paths: HashMap<String, PathBuf>,
    fans: HashMap<String, fan::ControlledFan>,
    curves: HashMap<String, Vec<Point>>,
    min_change: isize,
    max_change: isize,
}

#[derive(Debug)]
struct Point {
    temp: i32,
    fan_speed: u8,
}

impl State {
    fn new(config: Config) -> Result<Self, error::Error> {
        let span = info_span!("load");
        let _guard = span.enter();

        let n_hwmons = read_dir("/sys/class/hwmon")?.count();
        let mut hwmon_names = (0..n_hwmons)
            .map(|n| read_to_string(format!("/sys/class/hwmon/hwmon{}/name", n)))
            .collect::<Result<Vec<_>, _>>()?;
        for name in hwmon_names.iter_mut() {
            name.truncate(name.len() - 1);
        }
        tracing::debug!("found hwmons: {:?}", hwmon_names);

        let sensor_paths = config.find_sensors(&hwmon_names)?;
        let fans = config.find_fans(&hwmon_names)?;
        let curves = config.parse_curves()?;
        let min_change = config.min_change as isize * 255 / 100;
        let max_change = config.max_change as isize * 255 / 100;

        Ok(State {
            config,
            sensor_paths,
            fans,
            curves,
            min_change,
            max_change,
        })
    }
}

fn curve_lerp(temp: i32, curve: &[Point]) -> u8 {
    let span = trace_span!("curve lerp");
    let _guard = span.enter();
    if temp < curve[0].temp {
        return curve[0].fan_speed;
    }
    for window in curve.windows(2) {
        let (lower, upper) = (&window[0], &window[1]);
        if temp >= lower.temp && temp < upper.temp {
            trace!(?lower, ?upper, "temp in window");
            let normalised_temp = (temp - lower.temp) as isize;
            let upscale_factor = (upper.fan_speed - lower.fan_speed) as isize;
            let downscale_factor = (upper.temp - lower.temp) as isize;
            let fan_speed =
                normalised_temp * upscale_factor / downscale_factor + lower.fan_speed as isize;
            return fan_speed as u8;
        }
    }
    return curve[curve.len() - 1].fan_speed;
}

fn main_loop(stop: Arc<AtomicBool>, reload: Arc<AtomicBool>) -> Result<(), Error> {
    let config = Config::load()?;
    let mut state = State::new(config)?;
    while !stop.load(Ordering::Relaxed) {
        if reload.load(Ordering::Relaxed) {
            info!("attempting reload...");
            let new_config = match Config::load() {
                Ok(v) => v,
                Err(error) => {
                    error!(
                        ?error,
                        "failed to load new config - continuing with old one"
                    );
                    continue;
                }
            };
            let old_config = state.config;
            // reset fans - we don't know the new config works, but can't have the same fan open
            // twice
            state.fans = HashMap::new();
            match State::new(new_config) {
                Ok(new_state) => state = new_state,
                Err(e) => {
                    error!(?e, "failed to reload state - loading state from old config");
                    state = State::new(old_config)?;
                }
            };
            reload.store(false, Ordering::Relaxed);
        }

        let mut temps =
            HashMap::with_capacity(state.sensor_paths.len() + state.config.composites.len());
        for (name, path) in state.sensor_paths.iter() {
            let span = debug_span!(
                "reading sensor",
                name = name.as_str(),
                path = path.to_str().unwrap()
            );
            let _guard = span.enter();
            let temp: i32 = read_to_string(&path)?
                .trim()
                .parse()
                .map_err(Error::InvalidReading)?;
            debug!(temp, "read temperature");
            temps.insert(name, temp);
        }

        for (name, composite) in state.config.composites.iter() {
            let span = debug_span!("calculating composite", name = name.as_str());
            let _guard = span.enter();
            let mut inputs = Vec::with_capacity(composite.inputs.len());
            for input_name in composite.inputs.iter() {
                match temps.get(&input_name) {
                    Some(v) => inputs.push(*v),
                    None => {
                        warn!(name = input_name.as_str(), "input not found");
                        continue;
                    }
                }
            }

            if inputs.len() == 0 {
                warn!("no inputs");
                continue;
            }

            let pseudo_temp = match &composite.mode {
                CompositeMode::Max => inputs.iter().max().unwrap(),
                _ => todo!(),
            };

            temps.insert(&name, *pseudo_temp);
        }

        for (name, fan) in state.config.fans.iter() {
            let span = debug_span!("controlling fan", name = name.as_str(), input = Empty);
            let _guard = span.enter();
            let input_temp = match temps.get(&fan.input) {
                Some(v) => v,
                None => {
                    warn!(input = fan.input.as_str(), "input not found");
                    continue;
                }
            };
            span.record("input", input_temp);
            let curve = match state.curves.get(&fan.curve) {
                Some(v) => v,
                None => {
                    warn!(curve = fan.curve.as_str(), "curve not found");
                    continue;
                }
            };
            let target_speed = curve_lerp(*input_temp, curve);
            debug!(target_speed, "calculated target fan speed");

            let fan = state.fans.get(name).unwrap();
            let current_speed = fan.get_speed()? as isize;
            let mut delta = target_speed as isize - current_speed;
            if !(delta > state.min_change || delta < -state.min_change) {
                debug!(delta, "delta is too small - not changing speed");
                continue;
            }
            match delta.signum() {
                1 => delta = delta.clamp(0, state.max_change),
                -1 => delta = delta.clamp(-state.max_change, 0),
                _ => unreachable!(),
            }
            debug!(delta, "changing speed");
            fan.set_speed((current_speed + delta) as u8)?;
        }

        std::thread::sleep(Duration::from_millis(state.config.poll_period));
    }
    Ok(())
}

fn main() -> Result<(), Error> {
    tracing_subscriber::fmt::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    info!("hello!");

    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&stop))?;
    signal_hook::flag::register(SIGINT, Arc::clone(&stop))?;
    let reload = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGHUP, Arc::clone(&reload))?;
    signal_hook::flag::register(SIGUSR1, Arc::clone(&reload))?;

    while !stop.load(Ordering::Relaxed) {
        match main_loop(Arc::clone(&stop), Arc::clone(&reload)) {
            Ok(()) => break,
            Err(e) => {
                error!("encountered error in main loop:\n{}", e);
                info!("waiting {}ms and reloading", RETRY_MS);
                std::thread::sleep(Duration::from_millis(RETRY_MS));
            }
        }
    }

    info!("shutting down...");
    Ok(())
}
