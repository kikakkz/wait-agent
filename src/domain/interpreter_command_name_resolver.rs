use crate::domain::agent_detector::first_argv_token;

/// Resolves a meaningful display name for processes launched through an
/// interpreter or wrapper.
///
/// Examples:
/// - `python script.py` -> `script.py`
/// - `python -m module` -> `module`
/// - `node app.js`      -> `app.js`
/// - pip-installed console script `/.../bin/alter` -> `alter`
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterpreterCommandNameResolver;

impl InterpreterCommandNameResolver {
    pub fn new() -> Self {
        Self
    }

    /// Returns a display name derived from the process argv when the current
    /// command is an interpreter or wrapper.
    pub fn resolve(&self, current_command: &str, argv: Option<&[String]>) -> Option<String> {
        let argv = argv?;
        if argv.is_empty() {
            return None;
        }

        let argv0_base = first_argv_token(&argv[0]);

        // Case 1: wrapper/entry-point script (e.g. pip console scripts).
        // The binary name is meaningful, but the kernel reports the interpreter
        // as the current command because the wrapper's shebang points to it.
        // e.g. argv[0] = "/home/user/.local/bin/alter", current_command = "python"
        if !argv0_base.is_empty()
            && argv0_base != current_command
            && !crate::domain::agent_detector::SHELL_NAMES.contains(&argv0_base)
            && !is_known_interpreter(argv0_base)
        {
            return Some(argv0_base.to_string());
        }

        // Case 2: interpreter running a script or module directly.
        match current_command {
            "python" | "python3" | "python2" => self.resolve_python(argv),
            "node" | "nodejs" => self.resolve_node(argv),
            "ruby" => self.resolve_ruby(argv),
            _ => None,
        }
    }

    fn resolve_python(&self, argv: &[String]) -> Option<String> {
        // python -m module -> module
        if argv.get(1)?.as_str() == "-m" {
            return argv.get(2).cloned();
        }
        // python script.py -> script.py
        argv.get(1)
            .map(|path| std::path::Path::new(path))
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
    }

    fn resolve_node(&self, argv: &[String]) -> Option<String> {
        argv.get(1)
            .map(|path| std::path::Path::new(path))
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
    }

    fn resolve_ruby(&self, argv: &[String]) -> Option<String> {
        argv.get(1)
            .map(|path| std::path::Path::new(path))
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
    }
}

fn is_known_interpreter(name: &str) -> bool {
    matches!(
        name,
        "python" | "python3" | "python2" | "node" | "nodejs" | "ruby" | "perl" | "php"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_pip_console_script_from_argv0() {
        let resolver = InterpreterCommandNameResolver::new();
        let argv = vec!["/home/user/.local/bin/alter".to_string()];
        assert_eq!(
            resolver.resolve("python", Some(&argv)),
            Some("alter".to_string())
        );
    }

    #[test]
    fn resolves_python_script() {
        let resolver = InterpreterCommandNameResolver::new();
        let argv = vec!["python3".to_string(), "/path/to/script.py".to_string()];
        assert_eq!(
            resolver.resolve("python3", Some(&argv)),
            Some("script.py".to_string())
        );
    }

    #[test]
    fn resolves_python_module() {
        let resolver = InterpreterCommandNameResolver::new();
        let argv = vec![
            "python".to_string(),
            "-m".to_string(),
            "my_module".to_string(),
        ];
        assert_eq!(
            resolver.resolve("python", Some(&argv)),
            Some("my_module".to_string())
        );
    }

    #[test]
    fn resolves_node_script() {
        let resolver = InterpreterCommandNameResolver::new();
        let argv = vec!["node".to_string(), "/path/to/app.js".to_string()];
        assert_eq!(
            resolver.resolve("node", Some(&argv)),
            Some("app.js".to_string())
        );
    }

    #[test]
    fn leaves_python_repl_unchanged() {
        let resolver = InterpreterCommandNameResolver::new();
        let argv = vec!["python3".to_string()];
        assert_eq!(resolver.resolve("python3", Some(&argv)), None);
    }

    #[test]
    fn leaves_shell_unchanged() {
        let resolver = InterpreterCommandNameResolver::new();
        let argv = vec!["/bin/bash".to_string(), "script.sh".to_string()];
        assert_eq!(resolver.resolve("bash", Some(&argv)), None);
    }

    #[test]
    fn ignores_embedded_spaces_in_argv_zero() {
        let resolver = InterpreterCommandNameResolver::new();
        // Chrome sometimes embeds profile name and flags in argv[0]. The
        // resolver should only consider the leading executable token.
        let argv = vec!["google-chrome-replier --disable-gpu".to_string()];
        assert_eq!(resolver.resolve("google-chrome-replier", Some(&argv)), None);
    }
}
