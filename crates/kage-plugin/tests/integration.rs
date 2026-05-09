//! End-to-end test of the lua extension API: load a single fixture plugin
//! that exercises every registration surface (`on`, `register_tool`,
//! `register_command`, `register_provider`, `fs`, `http`, `notify`,
//! `log`) and verify the host can drive each one.

use std::path::PathBuf;

use kage_core::{CancelFlag, Content, Message, Role};
use kage_plugin::{HostLog, LogLevel, PluginRuntime, SharedHostLog};
use kage_provider::Provider;
use kage_tools::ToolContext;
use serde_json::json;

#[derive(Default)]
struct Recording {
    notifies: Vec<String>,
    logs: Vec<(LogLevel, String)>,
}

#[derive(Clone)]
struct Forwarder(std::sync::Arc<std::sync::Mutex<Recording>>);

impl HostLog for Forwarder {
    fn notify(&mut self, message: &str) {
        self.0.lock().unwrap().notifies.push(message.to_owned());
    }
    fn log(&mut self, level: LogLevel, message: &str) {
        self.0
            .lock()
            .unwrap()
            .logs
            .push((level, message.to_owned()));
    }
}

fn make_runtime(workdir: PathBuf) -> (std::sync::Arc<std::sync::Mutex<Recording>>, PluginRuntime) {
    let rec = std::sync::Arc::new(std::sync::Mutex::new(Recording::default()));
    let sink: SharedHostLog = std::sync::Arc::new(std::sync::Mutex::new(Box::new(Forwarder(
        rec.clone(),
    ))
        as Box<dyn HostLog + Send>));
    let rt = PluginRuntime::builder()
        .sink(sink)
        .config(json!({"name": "kage", "model": "anthropic:test"}))
        .workdir(workdir)
        .build()
        .unwrap();
    (rec, rt)
}

const FIXTURE: &str = r"
-- Track config readback.
captured_model = kage.config().model

-- Event: count message_end occurrences.
counter = 0
kage.on('message_end', function(ev)
    counter = counter + (ev.usage and ev.usage.output or 0)
end)

-- Tool: echo back the message field.
kage.register_tool({
    name = 'plugin_echo',
    description = 'echoes input.msg',
    schema = { type = 'object' },
    risk = 'read',
    execute = function(input) return 'echo:' .. (input.msg or '?') end,
})

-- Slash command: report the args verbatim.
kage.register_command({
    name = 'shout',
    description = 'shout the args back',
    handler = function(args) return 'SHOUT:' .. args end,
})

-- Provider: deterministic three-event stream.
kage.register_provider({
    id = 'fakeprov',
    stream = function(req)
        return {
            { type = 'message_start' },
            { type = 'text_delta', delta = 'model=' .. req.model },
            { type = 'message_end', stop_reason = 'end_turn',
              usage = { input = 0, output = 0, cache_read = 0, cache_write = 0 } },
        }
    end,
})

-- fs: write a file inside the sandboxed workdir, then read it.
kage.fs.write('round-trip.txt', 'hello from lua')
fs_round_trip = kage.fs.read('round-trip.txt')

-- http: every loopback URL must be rejected.
http_blocked = pcall(kage.http.get, 'http://127.0.0.1:1/x') == false

-- notify and log surface through the host sink.
kage.notify('plugin loaded')
kage.log('info', 'fixture done')
";

#[test]
fn fixture_plugin_drives_every_surface() {
    let dir = tempfile::tempdir().unwrap();
    let (rec, rt) = make_runtime(dir.path().to_path_buf());
    rt.eval(FIXTURE).expect("fixture loads cleanly");

    // kage.config visible from Lua.
    let captured: String = rt
        .eval("return captured_model")
        .unwrap()
        .as_string()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(captured, "anthropic:test");

    // kage.on counter accumulates as the host dispatches.
    rt.dispatch_event("message_end", &json!({"usage": {"output": 12}}))
        .unwrap();
    rt.dispatch_event("message_end", &json!({"usage": {"output": 8}}))
        .unwrap();
    let counter: i64 = rt.eval("return counter").unwrap().as_integer().unwrap();
    assert_eq!(counter, 20);

    // register_tool: invoke through the Tool trait the host would route into.
    let tools = rt.registered_tools();
    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    let cancel = CancelFlag::new();
    let workdir = dir.path().to_path_buf();
    let cx = ToolContext::new(&workdir, &cancel);
    let out = tool.execute(json!({"msg": "ping"}), &cx).unwrap();
    assert_eq!(out.text, "echo:ping");

    // register_command.
    let commands = rt.registered_commands();
    assert_eq!(commands.len(), 1);
    let cmd = &commands[0];
    let res = cmd.invoke("hello world", &json!(null)).unwrap();
    assert_eq!(res.text, "SHOUT:hello world");

    // register_provider: stream returns the table-of-events.
    let providers = rt.registered_providers();
    assert_eq!(providers.len(), 1);
    let provider = &providers[0];
    let stream = provider
        .stream(
            kage_provider::StreamRequest::new(
                "real-model",
                vec![Message::new(
                    Role::User,
                    vec![Content::Text { text: "x".into() }],
                    None,
                )],
            ),
            &cancel,
        )
        .unwrap();
    let events: Vec<_> = stream.collect::<Result<_, _>>().unwrap();
    assert_eq!(events.len(), 3);
    assert!(matches!(
        &events[1],
        kage_provider::ProviderEvent::TextDelta { delta } if delta == "model=real-model"
    ));

    // fs round-trip.
    let on_disk = std::fs::read_to_string(dir.path().join("round-trip.txt")).unwrap();
    assert_eq!(on_disk, "hello from lua");
    let fs_value: String = rt
        .eval("return fs_round_trip")
        .unwrap()
        .as_string()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(fs_value, "hello from lua");

    // http rejected loopback URL.
    let blocked: bool = rt
        .eval("return http_blocked")
        .unwrap()
        .as_boolean()
        .unwrap();
    assert!(blocked);

    // notify + log went to the host sink.
    let r = rec.lock().unwrap();
    assert!(r.notifies.iter().any(|s| s == "plugin loaded"));
    assert!(
        r.logs
            .iter()
            .any(|(lvl, msg)| *lvl == LogLevel::Info && msg == "fixture done")
    );
}
