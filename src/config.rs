use std::{
    collections::HashMap,
    fs::{read_dir, read_to_string, File},
    io::Read,
    path::PathBuf,
};

use serde::Deserialize;
use tracing::{debug, debug_span, trace};

use crate::{error::Error, fan::ControlledFan, Point};

#[derive(Deserialize)]
pub struct Config {
    pub poll_period: u64,
    pub min_change: usize,
    pub max_change: usize,
    sensors: HashMap<String, Sensor>,
    pub composites: HashMap<String, Composite>,
    curves: HashMap<String, Vec<String>>,
    pub fans: HashMap<String, Fan>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Sensor {
    ByNameLabel { hwmon_name: String, label: String },
    ByNameIndex { hwmon_name: String, index: usize },
}

#[derive(Deserialize)]
pub struct Composite {
    pub inputs: Vec<String>,
    #[serde(flatten)]
    pub mode: CompositeMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(tag = "mode")]
pub enum CompositeMode {
    Mean,
    Max,
    MeanMax { threshold: i32 },
}

#[derive(Deserialize)]
pub struct Fan {
    path: FanPath,
    pub input: String,
    pub curve: String,
}

#[derive(Deserialize)]
struct FanPath {
    hwmon_name: String,
    index: usize,
}

impl Config {
    pub(crate) fn load() -> Result<Self, Error> {
        let mut file = File::open("/etc/whoosh.toml")?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let config = toml::from_str(&contents)?;
        Ok(config)
    }

    pub(crate) fn parse_curves(&self) -> Result<HashMap<String, Vec<Point>>, Error> {
        let span = debug_span!("parsing curves");
        let _guard = span.enter();
        let mut ret = HashMap::with_capacity(self.curves.len());
        for (name, curve_spec) in self.curves.iter() {
            let mut curve = Vec::<Point>::with_capacity(curve_spec.len());
            for point_spec in curve_spec.iter() {
                trace!(point_spec = point_spec.as_str(), "parsing point_spec...");
                let (temp, fan_speed) = {
                    let mut iter = point_spec.split('/');
                    let temp: i32 = iter
                        .next()
                        .ok_or(Error::InvalidPointSpec)?
                        .trim_end_matches('C')
                        .parse()
                        .map_err(|_| Error::InvalidPointSpec)?;
                    let fan_percent: i32 = iter
                        .next()
                        .ok_or(Error::InvalidPointSpec)?
                        .trim_end_matches('%')
                        .parse()
                        .map_err(|_| Error::InvalidPointSpec)?;
                    // linux works in milli degrees celsius, and 0-255 fan speed
                    (temp * 1000, (fan_percent * 255 / 100) as u8)
                };
                curve.push(Point { temp, fan_speed });
            }
            ret.insert(name.clone(), curve);
        }
        Ok(ret)
    }

    pub(crate) fn find_sensors(
        &self,
        hwmon_names: &[String],
    ) -> Result<HashMap<String, PathBuf>, Error> {
        let span = debug_span!("finding sensors");
        let _guard = span.enter();
        let mut sensor_paths = HashMap::new();
        for (name, sensor) in self.sensors.iter() {
            match sensor {
                Sensor::ByNameLabel { hwmon_name, label } => {
                    let span = debug_span!(
                        "sensor",
                        hwmon_name = hwmon_name.as_str(),
                        label = label.as_str()
                    );
                    let _guard = span.enter();
                    let (hwmon_index, _) = hwmon_names
                        .iter()
                        .enumerate()
                        .find(|(_i, name)| *name == hwmon_name)
                        .ok_or_else(|| Error::HwmonNameNotFound(hwmon_name.clone()))?;
                    let mut sensor_index = None;

                    for entry in read_dir(format!("/sys/class/hwmon/hwmon{}/", hwmon_index))? {
                        let _span = debug_span!("checking entry");
                        let entry = entry?;
                        let os_file_name = entry.file_name();
                        let file_name = os_file_name.to_str().unwrap();

                        if !file_name.starts_with("temp") || !file_name.ends_with("_label") {
                            continue;
                        }
                        debug!(file_name, "found temp sensor");

                        let this_label = read_to_string(entry.path())?.trim().to_owned();
                        if this_label == *label {
                            let index: usize = file_name
                                .trim_start_matches("temp")
                                .trim_end_matches("_label")
                                .parse()
                                .unwrap();
                            sensor_index = Some(index);
                        }
                    }

                    if sensor_index.is_none() {
                        return Err(Error::HwmonSensorNotFound);
                    }

                    let path = PathBuf::from(format!(
                        "/sys/class/hwmon/hwmon{}/temp{}_input",
                        hwmon_index,
                        sensor_index.unwrap()
                    ));
                    if !path.exists() {
                        panic!("sensor has label but no input");
                    }
                    sensor_paths.insert(name.clone(), path);
                }
                Sensor::ByNameIndex { hwmon_name, index } => {
                    let (hwmon_index, _) = hwmon_names
                        .iter()
                        .enumerate()
                        .find(|(_i, name)| *name == hwmon_name)
                        .ok_or_else(|| Error::HwmonNameNotFound(hwmon_name.clone()))?;

                    let path = PathBuf::from(format!(
                        "/sys/class/hwmon/hwmon{}/temp{}_input",
                        hwmon_index, index
                    ));
                    if !path.exists() {
                        return Err(Error::HwmonSensorNotFound);
                    }
                    sensor_paths.insert(name.clone(), path);
                }
            }
        }
        Ok(sensor_paths)
    }

    pub(crate) fn find_fans(
        &self,
        hwmon_names: &[String],
    ) -> Result<HashMap<String, ControlledFan>, Error> {
        let span = debug_span!("finding fans");
        let _guard = span.enter();
        let mut fans = HashMap::new();
        for (name, fan) in self.fans.iter() {
            let FanPath {
                ref hwmon_name,
                index,
            } = fan.path;
            let span = debug_span!("fan", hwmon_name = hwmon_name.as_str(), index);
            let _guard = span.enter();
            let (hwmon_index, _) = hwmon_names
                .iter()
                .enumerate()
                .find(|(_i, name)| *name == hwmon_name)
                .ok_or_else(|| Error::HwmonNameNotFound(hwmon_name.clone()))?;

            let fan = ControlledFan::new(format!(
                "/sys/class/hwmon/hwmon{}/pwm{}",
                hwmon_index, index
            ))?;
            fans.insert(name.clone(), fan);
        }

        Ok(fans)
    }
}
