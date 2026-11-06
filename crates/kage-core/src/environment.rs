//! The environment a session's processes start with.
//!
//! A standalone engine inherits its own process environment. A shared host
//! serves clients started from different shells, so a session there uses
//! the environment of the client that opened it: its virtualenv, its
//! `direnv` variables and its `PATH`.

use std::ffi::OsString;
use std::process::Command;
use std::sync::Arc;

/// The variables a session's shell commands and stdio servers start with.
/// Debug output names how many variables there are, never their values,
/// which may hold secrets.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Environment(Option<Arc<Vec<(OsString, OsString)>>>);

impl Environment {
    /// This process's own environment, read when a process starts.
    #[must_use]
    pub fn inherit() -> Self {
        Self(None)
    }

    /// A fixed set of variables, such as those a client sent.
    #[must_use]
    pub fn captured(vars: Vec<(OsString, OsString)>) -> Self {
        Self(Some(Arc::new(vars)))
    }

    /// Whether this is a captured set rather than the process environment.
    #[must_use]
    pub fn is_captured(&self) -> bool {
        self.0.is_some()
    }

    /// The variables, read from the process when inherited.
    #[must_use]
    pub fn vars(&self) -> Vec<(OsString, OsString)> {
        match &self.0 {
            Some(vars) => vars.as_ref().clone(),
            None => std::env::vars_os().collect(),
        }
    }

    /// The value of the variable `name`, read from the process when
    /// inherited.
    #[must_use]
    pub fn var(&self, name: &str) -> Option<OsString> {
        match &self.0 {
            Some(vars) => vars
                .iter()
                .find(|(key, _)| key.as_os_str() == name)
                .map(|(_, value)| value.clone()),
            None => std::env::var_os(name),
        }
    }

    /// Make `command` start with exactly these variables. An inherited
    /// environment leaves `command` unchanged.
    pub fn apply(&self, command: &mut Command) {
        if let Some(vars) = &self.0 {
            command.env_clear();
            command.envs(vars.iter().map(|(name, value)| (name, value)));
        }
    }
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(vars) => write!(f, "Environment::captured({} variables)", vars.len()),
            None => f.write_str("Environment::inherit"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_captured_environment_replaces_the_process_environment() {
        let env = Environment::captured(vec![("ONLY".into(), "this".into())]);
        let mut command = Command::new("env");
        env.apply(&mut command);
        let output = command.output().unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "ONLY=this\n");
        assert_eq!(format!("{env:?}"), "Environment::captured(1 variables)");
        assert_eq!(env.var("ONLY"), Some("this".into()));
        assert_eq!(env.var("PATH"), None);
    }

    #[test]
    fn an_inherited_environment_leaves_the_command_alone() {
        let env = Environment::inherit();
        let mut command = Command::new("true");
        env.apply(&mut command);
        assert_eq!(command.get_envs().count(), 0);
        assert!(!env.is_captured());
    }
}
