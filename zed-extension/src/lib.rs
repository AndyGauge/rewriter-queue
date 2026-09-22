use zed_extension_api::{self as zed, settings::ContextServerSettings, Result};

const SERVER_ID: &str = "rewriter-queue";

const INSTALLATION_INSTRUCTIONS: &str = "\
This extension launches the local `rewriter-queue` client, which talks to the queue server.
Build it, then point Zed at it in `settings.json`:

```
cargo build -p job-queue
```

```json
{
  \"context_servers\": {
    \"rewriter-queue\": {
      \"command\": { \"path\": \"/path/to/rewriter/target/debug/rewriter-queue\" }
    }
  }
}
```

The server address is read from `[queue] url` in `~/.config/rewriter/config.toml` and the token from
`~/.config/rewriter/queue-token`. Without a path above, `rewriter-queue` is looked up on `PATH`.
For a live queue view, run the \"Queue: watch\" task.
";

struct RewriterQueue;

impl zed::Extension for RewriterQueue {
    fn new() -> Self {
        Self
    }

    fn context_server_command(
        &mut self,
        _context_server_id: &zed::ContextServerId,
        project: &zed::Project,
    ) -> Result<zed::Command> {
        let configured = ContextServerSettings::for_project(SERVER_ID, project)?.command;
        let path = configured
            .as_ref()
            .and_then(|c| c.path.clone())
            .unwrap_or_else(|| SERVER_ID.to_string());
        let mut args = configured
            .as_ref()
            .and_then(|c| c.arguments.clone())
            .unwrap_or_default();
        if args.is_empty() {
            args.push("mcp".to_string());
        }
        let env = configured
            .and_then(|c| c.env)
            .map(|e| e.into_iter().collect())
            .unwrap_or_default();

        Ok(zed::Command { command: path, args, env })
    }

    fn context_server_configuration(
        &mut self,
        _context_server_id: &zed::ContextServerId,
        _project: &zed::Project,
    ) -> Result<Option<zed::ContextServerConfiguration>> {
        Ok(Some(zed::ContextServerConfiguration {
            installation_instructions: INSTALLATION_INSTRUCTIONS.to_string(),
            settings_schema: "{}".to_string(),
            default_settings: "{}".to_string(),
        }))
    }
}

zed::register_extension!(RewriterQueue);
