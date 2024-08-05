#[derive(Debug, PartialEq)]
pub enum BuildModel {
    Debug,
    Release,
}

pub fn build_channel() -> BuildModel {
    if cfg!(debug_assertions) {
        return BuildModel::Debug;
    }
    BuildModel::Release
}

impl ToString for BuildModel {
    fn to_string(&self) -> String {
        match self {
            Self::Debug => "debug".to_string(),
            Self::Release => "release".to_string(),
        }
    }
}

pub fn is_debug() -> bool {
    build_channel() == BuildModel::Debug
}

pub fn is_release() -> bool {
    build_channel() == BuildModel::Release
}
