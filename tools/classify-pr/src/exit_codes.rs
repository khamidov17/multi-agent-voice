/// Exit codes are the GitHub Action's primary signal. The workflow reads the
/// process exit code to decide label + status check; the JSON verdict on
/// stdout is for humans, reconciler, and canary.
///
/// Never renumber. Workflows + docs reference these constants by value.
pub const ELIGIBLE: i32 = 0;
pub const INELIGIBLE: i32 = 1;
pub const OPERATIONAL_ERROR: i32 = 2;
pub const PROTECTED_PATH: i32 = 3;
pub const TOOLCHAIN_DRIFT: i32 = 4;
pub const PAUSED: i32 = 5;
