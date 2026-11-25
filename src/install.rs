//! `machinestream-mcp install <client>`: write, or print, the MCP configuration that starts
//! this server in a client.
//!
//! Each client keeps its servers in its own file and shape. An existing file is merged
//! into, never replaced: other servers and settings stay as they are, and a file that is
//! not plain JSON (or TOML, for Codex) is left alone and the entry is printed instead.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The name this server is registered under in a client.
pub const ENTRY_NAME: &str = "machinestream";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Client {
    ClaudeCode,
    Cursor,
    Codex,
    Vscode,
    Windsurf,
    GeminiCli,
    ClaudeDesktop,
}

impl Client {
    pub const ALL: [Client; 7] = [
        Client::ClaudeCode,
        Client::Cursor,
        Client::Codex,
        Client::Vscode,
        Client::Windsurf,
        Client::GeminiCli,
        Client::ClaudeDesktop,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Codex => "Codex",
            Self::Vscode => "VS Code",
            Self::Windsurf => "Windsurf",
            Self::GeminiCli => "Gemini CLI",
            Self::ClaudeDesktop => "Claude Desktop",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Cursor => "cursor",
            Self::Codex => "codex",
            Self::Vscode => "vscode",
            Self::Windsurf => "windsurf",
            Self::GeminiCli => "gemini-cli",
            Self::ClaudeDesktop => "claude-desktop",
        }
    }

    /// What to do in the client after its configuration changed.
    pub fn after(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Restart Claude Code in this folder (or run /mcp) and approve the machinestream server.",
            Self::Cursor => "Open Cursor's MCP settings and enable machinestream, or restart Cursor.",
            Self::Codex => "Start a new Codex session.",
            Self::Vscode => "Open the MCP view in VS Code and start machinestream.",
            Self::Windsurf => "Refresh the MCP servers in Windsurf, or restart it.",
            Self::GeminiCli => "Run /mcp reload in Gemini CLI, or restart it.",
            Self::ClaudeDesktop => "Quit and reopen Claude Desktop.",
        }
    }
}

/// How the file holds its servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// JSON, servers under `mcpServers`.
    McpServers,
    /// JSON, servers under `servers`, each with `"type": "stdio"` (VS Code).
    VscodeServers,
    /// TOML, `[mcp_servers.<name>]` (Codex).
    CodexToml,
}

/// Where a client's configuration goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub client: Client,
    pub path: PathBuf,
    pub shape: Shape,
    /// The file belongs to the project (it may be committed and shared).
    pub project_scoped: bool,
}

/// The command that starts the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Launch {
    /// Through the npm launcher, which fetches the binary for the platform.
    pub fn npx() -> Self {
        Self {
            command: "npx".into(),
            args: vec!["-y".into(), "@contextstream/machinestream-mcp".into()],
            env: Vec::new(),
        }
    }

    /// This very binary, by its path.
    pub fn binary(path: &Path) -> Self {
        Self {
            command: path.to_string_lossy().into_owned(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }
}

/// The file `client` reads, for a project at `project` and a home folder at `home`.
/// `global` asks for the user-wide file where the client also has a project one.
pub fn target(
    client: Client,
    project: &Path,
    home: &Path,
    config_dir: &Path,
    global: bool,
) -> Target {
    let (path, shape, project_scoped) = match (client, global) {
        (Client::ClaudeCode, false) => (project.join(".mcp.json"), Shape::McpServers, true),
        (Client::ClaudeCode, true) => (home.join(".claude.json"), Shape::McpServers, false),
        (Client::Cursor, false) => (
            project.join(".cursor").join("mcp.json"),
            Shape::McpServers,
            true,
        ),
        (Client::Cursor, true) => (
            home.join(".cursor").join("mcp.json"),
            Shape::McpServers,
            false,
        ),
        (Client::Vscode, false) => (
            project.join(".vscode").join("mcp.json"),
            Shape::VscodeServers,
            true,
        ),
        (Client::Vscode, true) => (
            config_dir.join("Code").join("User").join("mcp.json"),
            Shape::VscodeServers,
            false,
        ),
        (Client::GeminiCli, false) => (
            project.join(".gemini").join("settings.json"),
            Shape::McpServers,
            true,
        ),
        (Client::GeminiCli, true) => (
            home.join(".gemini").join("settings.json"),
            Shape::McpServers,
            false,
        ),
        (Client::Codex, _) => (
            home.join(".codex").join("config.toml"),
            Shape::CodexToml,
            false,
        ),
        (Client::Windsurf, _) => (
            home.join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
            Shape::McpServers,
            false,
        ),
        (Client::ClaudeDesktop, _) => (
            config_dir.join("Claude").join("claude_desktop_config.json"),
            Shape::McpServers,
            false,
        ),
    };
    Target {
        client,
        path,
        shape,
        project_scoped,
    }
}

/// Whether `client` looks installed on this machine: its own folder or file is there, or
/// its command is on the PATH (`on_path`).
pub fn detected(
    client: Client,
    home: &Path,
    config_dir: &Path,
    on_path: &dyn Fn(&str) -> bool,
) -> bool {
    match client {
        Client::ClaudeCode => {
            home.join(".claude.json").exists() || home.join(".claude").is_dir() || on_path("claude")
        }
        Client::Cursor => home.join(".cursor").is_dir() || on_path("cursor"),
        Client::Codex => home.join(".codex").is_dir() || on_path("codex"),
        Client::Vscode => config_dir.join("Code").join("User").is_dir() || on_path("code"),
        Client::Windsurf => home.join(".codeium").join("windsurf").is_dir() || on_path("windsurf"),
        Client::GeminiCli => {
            home.join(".gemini").join("settings.json").exists() || on_path("gemini")
        }
        Client::ClaudeDesktop => config_dir.join("Claude").is_dir(),
    }
}

/// Whether a command called `name` is in one of the PATH's folders.
pub fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat"]
            .iter()
            .map(|extension| format!("{name}.{extension}"))
            .collect()
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&path).any(|folder| names.iter().any(|name| folder.join(name).is_file()))
}

/// The launch for `client`. A client that does not start servers in the project folder is
/// told the folder through the environment.
pub fn launch_for(client: Client, base: Launch, project: &Path) -> Launch {
    let mut launch = base;
    if client == Client::ClaudeDesktop {
        launch.env.push((
            "MACHINESTREAM_ROOT".into(),
            project.to_string_lossy().into_owned(),
        ));
    }
    launch
}

/// The server's entry in a JSON configuration.
pub fn json_entry(shape: Shape, launch: &Launch) -> Value {
    let mut entry = Map::new();
    if shape == Shape::VscodeServers {
        entry.insert("type".into(), json!("stdio"));
    }
    entry.insert("command".into(), json!(launch.command));
    entry.insert("args".into(), json!(launch.args));
    if !launch.env.is_empty() {
        entry.insert(
            "env".into(),
            Value::Object(
                launch
                    .env
                    .iter()
                    .map(|(name, value)| (name.clone(), json!(value)))
                    .collect(),
            ),
        );
    }
    Value::Object(entry)
}

fn servers_key(shape: Shape) -> &'static str {
    match shape {
        Shape::VscodeServers => "servers",
        _ => "mcpServers",
    }
}

/// What to paste into the client's file by hand.
pub fn snippet(target: &Target, launch: &Launch) -> String {
    match target.shape {
        Shape::CodexToml => codex_block(launch),
        shape => {
            let mut servers = Map::new();
            servers.insert(ENTRY_NAME.into(), json_entry(shape, launch));
            let mut root = Map::new();
            root.insert(servers_key(shape).into(), Value::Object(servers));
            serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_default()
        }
    }
}

fn toml_string(text: &str) -> String {
    toml_edit::value(text).to_string()
}

fn codex_block(launch: &Launch) -> String {
    let args: Vec<String> = launch.args.iter().map(|arg| toml_string(arg)).collect();
    let mut block = format!(
        "[mcp_servers.{ENTRY_NAME}]\ncommand = {}\nargs = [{}]\n",
        toml_string(&launch.command),
        args.join(", ")
    );
    if !launch.env.is_empty() {
        let env: Vec<String> = launch
            .env
            .iter()
            .map(|(name, value)| format!("{name} = {}", toml_string(value)))
            .collect();
        block.push_str(&format!("env = {{ {} }}\n", env.join(", ")));
    }
    block
}

/// `existing` (the file's text, if it exists) with the server's entry merged in. `Ok(None)`
/// when it already says the same.
pub fn merged(
    target: &Target,
    launch: &Launch,
    existing: Option<&str>,
) -> Result<Option<String>, String> {
    match target.shape {
        Shape::CodexToml => merged_toml(launch, existing),
        shape => merged_json(shape, launch, existing),
    }
}

fn merged_json(
    shape: Shape,
    launch: &Launch,
    existing: Option<&str>,
) -> Result<Option<String>, String> {
    let mut root = match existing.map(str::trim).filter(|text| !text.is_empty()) {
        None => Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(root)) => root,
            Ok(_) => return Err("the file is not a JSON object".to_string()),
            Err(_) => {
                return Err(
                    "the file is not plain JSON (it may hold comments or trailing commas)"
                        .to_string(),
                )
            }
        },
    };
    let servers = root
        .entry(servers_key(shape))
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        return Err(format!(
            "{} in the file is not an object",
            servers_key(shape)
        ));
    };
    let entry = json_entry(shape, launch);
    if servers.get(ENTRY_NAME) == Some(&entry) {
        return Ok(None);
    }
    servers.insert(ENTRY_NAME.into(), entry);
    let mut text =
        serde_json::to_string_pretty(&Value::Object(root)).map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(Some(text))
}

fn merged_toml(launch: &Launch, existing: Option<&str>) -> Result<Option<String>, String> {
    let mut document = existing
        .unwrap_or("")
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| "the file is not valid TOML".to_string())?;
    let before = document.to_string();
    let servers = document
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        })
        .as_table_mut()
        .ok_or_else(|| "mcp_servers in the file is not a table".to_string())?;
    let mut entry = toml_edit::Table::new();
    entry["command"] = toml_edit::value(launch.command.as_str());
    let mut args = toml_edit::Array::new();
    for arg in &launch.args {
        args.push(arg.as_str());
    }
    entry["args"] = toml_edit::value(args);
    if !launch.env.is_empty() {
        let mut env = toml_edit::InlineTable::new();
        for (name, value) in &launch.env {
            env.insert(name, value.as_str().into());
        }
        entry["env"] = toml_edit::value(env);
    }
    servers.insert(ENTRY_NAME, toml_edit::Item::Table(entry));
    let after = document.to_string();
    Ok((after != before).then_some(after))
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Written(PathBuf),
    AlreadySet(PathBuf),
    /// The file was left alone; the reason, and what to add by hand.
    LeftAlone {
        path: PathBuf,
        reason: String,
        snippet: String,
    },
}

/// Merge the server's entry into the client's file.
pub fn install(target: &Target, launch: &Launch) -> std::io::Result<Outcome> {
    let existing = match std::fs::symlink_metadata(&target.path) {
        Ok(metadata) if metadata.is_file() => Some(std::fs::read_to_string(&target.path)?),
        Ok(_) => {
            return Ok(Outcome::LeftAlone {
                path: target.path.clone(),
                reason: "it is not a regular file".to_string(),
                snippet: snippet(target, launch),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let text = match merged(target, launch, existing.as_deref()) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(Outcome::AlreadySet(target.path.clone())),
        Err(reason) => {
            return Ok(Outcome::LeftAlone {
                path: target.path.clone(),
                reason,
                snippet: snippet(target, launch),
            })
        }
    };
    let parent = target
        .path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no folder"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(text.as_bytes())?;
    // Keep the permissions of a file that was there (a user-wide file may be private).
    if let Ok(metadata) = std::fs::metadata(&target.path) {
        let _ = std::fs::set_permissions(temporary.path(), metadata.permissions());
    }
    temporary
        .persist(&target.path)
        .map_err(|error| error.error)?;
    Ok(Outcome::Written(target.path.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        let config = home.join(".config");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        (dir, project, home, config)
    }

    #[test]
    fn every_client_has_a_target() {
        let (_dir, project, home, config) = paths();
        let expect = [
            (Client::ClaudeCode, false, project.join(".mcp.json")),
            (Client::ClaudeCode, true, home.join(".claude.json")),
            (Client::Cursor, false, project.join(".cursor/mcp.json")),
            (Client::Cursor, true, home.join(".cursor/mcp.json")),
            (Client::Codex, false, home.join(".codex/config.toml")),
            (Client::Vscode, false, project.join(".vscode/mcp.json")),
            (Client::Vscode, true, config.join("Code/User/mcp.json")),
            (
                Client::Windsurf,
                false,
                home.join(".codeium/windsurf/mcp_config.json"),
            ),
            (
                Client::GeminiCli,
                false,
                project.join(".gemini/settings.json"),
            ),
            (Client::GeminiCli, true, home.join(".gemini/settings.json")),
            (
                Client::ClaudeDesktop,
                false,
                config.join("Claude/claude_desktop_config.json"),
            ),
        ];
        for (client, global, path) in expect {
            assert_eq!(target(client, &project, &home, &config, global).path, path);
        }
        assert_eq!(Client::ALL.len(), 7);
    }

    #[test]
    fn clients_are_found_by_their_folders_or_their_commands() {
        let (_dir, _project, home, config) = paths();
        let nothing = |_: &str| false;
        for client in Client::ALL {
            assert!(!detected(client, &home, &config, &nothing), "{client:?}");
        }
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        std::fs::create_dir_all(home.join(".codeium/windsurf")).unwrap();
        std::fs::create_dir_all(config.join("Claude")).unwrap();
        std::fs::write(home.join(".claude.json"), "{}").unwrap();
        let found: Vec<Client> = Client::ALL
            .into_iter()
            .filter(|client| detected(*client, &home, &config, &nothing))
            .collect();
        assert_eq!(
            found,
            [
                Client::ClaudeCode,
                Client::Cursor,
                Client::Windsurf,
                Client::ClaudeDesktop
            ]
        );
        let codex_only = |name: &str| name == "codex";
        assert!(detected(Client::Codex, &home, &config, &codex_only));
        assert!(!detected(Client::GeminiCli, &home, &config, &codex_only));
    }

    #[test]
    fn a_new_json_file_holds_just_the_server() {
        let (_dir, project, home, config) = paths();
        let target = target(Client::ClaudeCode, &project, &home, &config, false);
        assert_eq!(
            install(&target, &Launch::npx()).unwrap(),
            Outcome::Written(project.join(".mcp.json"))
        );
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target.path).unwrap()).unwrap();
        assert_eq!(
            written,
            json!({"mcpServers": {"machinestream": {"command": "npx", "args": ["-y", "@contextstream/machinestream-mcp"]}}})
        );
        assert_eq!(
            install(&target, &Launch::npx()).unwrap(),
            Outcome::AlreadySet(project.join(".mcp.json")),
            "a second install changes nothing"
        );
    }

    #[test]
    fn other_servers_and_settings_are_kept() {
        let (_dir, project, home, config) = paths();
        let target = target(Client::GeminiCli, &project, &home, &config, true);
        std::fs::create_dir_all(target.path.parent().unwrap()).unwrap();
        std::fs::write(
            &target.path,
            r#"{"theme": "dark", "mcpServers": {"other": {"command": "other-server"}}}"#,
        )
        .unwrap();
        install(&target, &Launch::npx()).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target.path).unwrap()).unwrap();
        assert_eq!(written["theme"], "dark");
        assert_eq!(written["mcpServers"]["other"]["command"], "other-server");
        assert_eq!(written["mcpServers"]["machinestream"]["command"], "npx");
    }

    #[test]
    fn vs_code_uses_servers_and_a_type() {
        let (_dir, project, home, config) = paths();
        let target = target(Client::Vscode, &project, &home, &config, false);
        install(&target, &Launch::npx()).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target.path).unwrap()).unwrap();
        assert_eq!(
            written,
            json!({"servers": {"machinestream": {"type": "stdio", "command": "npx", "args": ["-y", "@contextstream/machinestream-mcp"]}}})
        );
    }

    #[test]
    fn codex_gets_a_toml_table_and_keeps_the_rest() {
        let (_dir, project, home, config) = paths();
        let target = target(Client::Codex, &project, &home, &config, false);
        std::fs::create_dir_all(target.path.parent().unwrap()).unwrap();
        std::fs::write(
            &target.path,
            "# my settings\nmodel = \"example\"\n\n[mcp_servers.other]\ncommand = \"other-server\"\n",
        )
        .unwrap();
        assert!(matches!(
            install(&target, &Launch::npx()).unwrap(),
            Outcome::Written(_)
        ));
        let text = std::fs::read_to_string(&target.path).unwrap();
        assert!(text.starts_with("# my settings\nmodel = \"example\"\n"));
        let parsed: toml_edit::DocumentMut = text.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"].as_str(),
            Some("other-server")
        );
        assert_eq!(
            parsed["mcp_servers"]["machinestream"]["command"].as_str(),
            Some("npx")
        );
        assert_eq!(
            parsed["mcp_servers"]["machinestream"]["args"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            install(&target, &Launch::npx()).unwrap(),
            Outcome::AlreadySet(target.path.clone())
        );
    }

    #[test]
    fn claude_desktop_is_told_the_project_folder() {
        let (_dir, project, home, config) = paths();
        let target = target(Client::ClaudeDesktop, &project, &home, &config, false);
        let launch = launch_for(Client::ClaudeDesktop, Launch::npx(), &project);
        install(&target, &launch).unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target.path).unwrap()).unwrap();
        assert_eq!(
            written["mcpServers"]["machinestream"]["env"]["MACHINESTREAM_ROOT"],
            project.to_string_lossy().as_ref()
        );
        assert!(launch_for(Client::Cursor, Launch::npx(), &project)
            .env
            .is_empty());
    }

    #[test]
    fn a_file_with_comments_is_left_alone_and_the_entry_printed() {
        let (_dir, project, home, config) = paths();
        let target = target(Client::Cursor, &project, &home, &config, false);
        std::fs::create_dir_all(target.path.parent().unwrap()).unwrap();
        let original = "{\n  // my servers\n  \"mcpServers\": {}\n}\n";
        std::fs::write(&target.path, original).unwrap();
        match install(&target, &Launch::npx()).unwrap() {
            Outcome::LeftAlone {
                reason, snippet, ..
            } => {
                assert!(reason.contains("not plain JSON"));
                assert!(snippet.contains("\"machinestream\""));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&target.path).unwrap(), original);
    }

    #[test]
    fn a_snippet_is_what_a_new_file_would_hold() {
        let (_dir, project, home, config) = paths();
        for client in Client::ALL {
            let target = target(client, &project, &home, &config, false);
            let launch = launch_for(
                client,
                Launch::binary(Path::new("/opt/bin/machinestream-mcp")),
                &project,
            );
            let text = snippet(&target, &launch);
            assert!(text.contains("machinestream"), "{client:?}");
            assert!(text.contains("/opt/bin/machinestream-mcp"), "{client:?}");
            let fresh = merged(&target, &launch, None).unwrap().unwrap();
            match target.shape {
                Shape::CodexToml => assert_eq!(fresh.trim(), text.trim(), "{client:?}"),
                _ => assert_eq!(
                    serde_json::from_str::<Value>(&fresh).unwrap(),
                    serde_json::from_str::<Value>(&text).unwrap(),
                    "{client:?}"
                ),
            }
        }
    }

    #[test]
    fn a_path_with_quotes_is_escaped_in_toml() {
        let launch = Launch::binary(Path::new("/odd \"dir\"/machinestream-mcp"));
        let block = codex_block(&launch);
        let parsed: toml_edit::DocumentMut = block.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["machinestream"]["command"].as_str(),
            Some("/odd \"dir\"/machinestream-mcp")
        );
    }
}
