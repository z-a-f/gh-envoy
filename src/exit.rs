#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EnvoyExitCode {
    Success = 0,
    Warning = 1,
    Blocked = 2,
    OperationalError = 3,
    Held = 4,
}

impl From<EnvoyExitCode> for std::process::ExitCode {
    fn from(value: EnvoyExitCode) -> Self {
        Self::from(value as u8)
    }
}
