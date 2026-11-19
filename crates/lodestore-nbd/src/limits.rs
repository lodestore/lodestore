use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub op_timeout: Duration,
    pub max_read: u32,
    pub max_export_size: u64,
    pub max_extents: usize,
    pub max_option_payload: usize,
    pub max_options: usize,
    pub max_error_message: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            op_timeout: Duration::from_secs(30),
            max_read: 4 << 20,
            max_export_size: i64::MAX as u64,
            max_extents: 65_536,
            max_option_payload: 4096,
            max_options: 32,
            max_error_message: 4096,
        }
    }
}
