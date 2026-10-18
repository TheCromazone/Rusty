//! Private worker transport. Stdout is never a protocol channel.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::io::{self, BufRead, Write};
#[cfg(test)]
use std::path::Path;

pub const VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

// Each field and its Swift spelling is declared together, so generated Swift
// cannot silently gain a different optionality or wire key.
macro_rules! wire_struct {
    ($(#[$meta:meta])* $name:ident { $($field:ident : $ty:ty => $swift:literal),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[allow(dead_code)] // Contract types also feed Swift and recorded-wire validation.
        $(#[$meta])*
        pub struct $name { $(pub $field: $ty),* }
        #[cfg(test)]
        impl $name {
            #[allow(dead_code)] // Only command params need strict key validation.
            fn swift_keys() -> &'static [&'static str] { &[$(stringify!($field)),*] }
            fn swift() -> String {
                let mut s = format!("public struct {}: Codable, Equatable, Sendable {{\n", stringify!($name));
                $(s.push_str(&format!("    public var `{}`: {}\n", stringify!($field), $swift));)*
                s.push_str("    public init(");
                let fields: &[&str] = &[$(concat!("`", stringify!($field), "`: ", $swift)),*];
                s.push_str(&fields.join(", "));
                s.push_str(") {\n");
                $(s.push_str(&format!("        self.{} = `{}`\n", stringify!($field), stringify!($field)));)*
                s.push_str("    }\n}\n\n"); s
            }
        }
    }
}
wire_struct!(Request { protocol:u32 => "UInt32", request_id:String => "String", method:String => "String", params:Value => "JSONValue" });
wire_struct!(Event { protocol:u32 => "UInt32", instance_id:String => "String", seq:u64 => "UInt64", kind:String => "String", payload:Value => "JSONValue" });
wire_struct!(#[serde(deny_unknown_fields)] SubmitParams { text:String => "String" });
wire_struct!(#[serde(deny_unknown_fields)] CancelParams { run_id:String => "String" });
wire_struct!(#[serde(deny_unknown_fields)] ApprovalParams { approval_id:String => "String", allowed:bool => "Bool", operation_digest:String => "String", typed:Option<String> => "String?" });
wire_struct!(#[serde(deny_unknown_fields)] InspectParams { operation:String => "String", arguments:Value => "JSONValue" });
wire_struct!(
    #[serde(deny_unknown_fields)]
    EmptyParams {}
);

// Event payloads are part of the same generated contract as commands. Dynamic
// provider/tool values remain JSON, but lifecycle and authority fields are typed.
wire_struct!(HelloPayload { protocol:u32 => "UInt32", schema_hash:String => "String", worker_instance_id:String => "String", inspect_only:bool => "Bool", phase:String => "String", protocol_capabilities:Vec<String> => "[String]" });
wire_struct!(ReadyPayload { session_id:String => "String", read_only:bool => "Bool", lease:Option<String> => "String?", reason:Option<String> => "String?" });
wire_struct!(StartupFailedPayload { code:String => "String", message:String => "String" });
wire_struct!(ClosingPayload { reason:String => "String" });
wire_struct!(PendingApprovalPayload { approval_id:String => "String", operation_digest:String => "String", run_id:String => "String", expires_in_secs:u64 => "UInt64", typed_required:bool => "Bool", typed_phrase:Option<String> => "String?", operation:Value => "JSONValue" });
wire_struct!(ToolFlightPayload { run_id:String => "String", tool_id:Option<String> => "String?", activity_id:Option<String> => "String?", tool:String => "String", summary:String => "String", requester:String => "String", started_at:u64 => "UInt64" });
wire_struct!(SnapshotPayload { worker_instance_id:String => "String", snapshot_watermark:u64 => "UInt64", session_id:String => "String", run_id:Option<String> => "String?", phase:String => "String", live_text:String => "String", live_reasoning:String => "String", live_truncated:bool => "Bool", last_tool:Value => "JSONValue", pending_approval:Option<PendingApprovalPayload> => "PendingApprovalPayload?", history:Value => "JSONValue", cost_usd:Option<f64> => "Double?", receipts:Value => "JSONValue", receipts_truncated:bool => "Bool", uncertain_run_count:usize => "Int", uncertain_runs:Vec<Value> => "[JSONValue]", uncertain_runs_truncated:bool => "Bool", uncertain_tools:Vec<ToolFlightPayload> => "[ToolFlightPayload]", uncertain_tool_count:usize => "Int", uncertain_tools_truncated:bool => "Bool", read_only:Option<bool> => "Bool?", recovery_unknown:Option<bool> => "Bool?", read_only_reason:Option<String> => "String?", in_flight_tools:Vec<ToolFlightPayload> => "[ToolFlightPayload]", in_flight_tools_truncated:bool => "Bool" });
wire_struct!(ApprovalRequestedPayload { approval_id:String => "String", operation_digest:String => "String", operation:Value => "JSONValue", expires_at:u64 => "UInt64", typed_phrase:Option<String> => "String?", sensitive:bool => "Bool" });
wire_struct!(ApprovalResolvedPayload { approval_id:String => "String", allowed:bool => "Bool" });
wire_struct!(ApprovalExpiredPayload { approval_id:String => "String" });
wire_struct!(RunStartedPayload { run_id:String => "String", request_id:String => "String" });
wire_struct!(RunCompletedPayload { run_id:String => "String", request_id:String => "String", state:String => "String", error:Option<String> => "String?", budget:Value => "JSONValue", cost_usd:Option<f64> => "Double?" });
wire_struct!(ToolProposedPayload { name:String => "String", arguments:Value => "JSONValue" });
wire_struct!(ToolStartedPayload { name:String => "String", arguments:Value => "JSONValue", requester:String => "String", tool_id:String => "String" });
wire_struct!(ToolCompletedPayload { name:String => "String", arguments:Value => "JSONValue", text:String => "String", ok:bool => "Bool" });
wire_struct!(ReplyPayload { request_id:String => "String", ok:bool => "Bool", result:Option<Value> => "JSONValue?", error:Option<String> => "String?" });
wire_struct!(ResyncPayload { reason:String => "String" });
wire_struct!(MessageDeltaPayload { text:String => "String" });
wire_struct!(ReasoningDeltaPayload { text:String => "String" });
wire_struct!(MessageCompletedPayload {});
wire_struct!(UsagePayload { prompt_tokens:u64 => "UInt64", completion_tokens:u64 => "UInt64", context_pct:usize => "Int", interrupted:bool => "Bool" });
wire_struct!(StatusPayload { text:String => "String" });
wire_struct!(NoticePayload { text:String => "String" });
wire_struct!(TruncatedPayload { original_kind:String => "String", name:Option<String> => "String?", truncated:bool => "Bool", summary:String => "String" });
wire_struct!(ProtocolErrorPayload { error:String => "String" });

macro_rules! payloads {
    ($($kind:literal => $ty:ident),* $(,)?) => {
        impl Event {
            #[cfg(any(test, debug_assertions))]
            pub fn validate_payload(&self) -> Result<(), serde_json::Error> {
                match self.kind.as_str() {
                    $($kind => { serde_json::from_value::<$ty>(self.payload.clone())?; },)*
                    _ => (), // Additive event kinds remain forward compatible.
                }
                Ok(())
            }
        }
        #[cfg(test)]
        fn swift_payloads() -> String {
            let mut s = String::new();
            $(s.push_str(&$ty::swift());)*
            s.push_str("public enum EventPayload: Equatable, Sendable {\n");
            $(s.push_str(&format!("    case {}({})\n", stringify!($ty), stringify!($ty)));)*
            s.push_str("    case unknown(String, JSONValue)\n}\n\npublic extension Event {\n    func decodedPayload() throws -> EventPayload {\n        let data = try JSONEncoder().encode(payload)\n        let decoder = JSONDecoder()\n        switch kind {\n");
            $(s.push_str(&format!("        case \"{}\": return .{}(try decoder.decode({}.self, from: data))\n", $kind, stringify!($ty), stringify!($ty)));)*
            s.push_str("        default: return .unknown(kind, payload)\n        }\n    }\n}\n\n");
            s
        }
    }
}
payloads! {
    "hello" => HelloPayload, "session.ready" => ReadyPayload,
    "startup.failed" => StartupFailedPayload, "worker.closing" => ClosingPayload,
    "session.snapshot" => SnapshotPayload, "approval.requested" => ApprovalRequestedPayload,
    "approval.resolved" => ApprovalResolvedPayload, "approval.expired" => ApprovalExpiredPayload,
    "run.started" => RunStartedPayload, "run.completed" => RunCompletedPayload,
    "tool.proposed" => ToolProposedPayload, "tool.started" => ToolStartedPayload,
    "tool.completed" => ToolCompletedPayload, "reply" => ReplyPayload,
    "resync_required" => ResyncPayload,
    "message.delta" => MessageDeltaPayload, "reasoning.delta" => ReasoningDeltaPayload,
    "message.completed" => MessageCompletedPayload, "run.usage" => UsagePayload,
    "run.status" => StatusPayload, "notice" => NoticePayload,
    "event.truncated" => TruncatedPayload, "protocol.error" => ProtocolErrorPayload,
}

macro_rules! commands {
    ($($variant:ident($params:ident) => $method:literal),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq)]
        pub enum Command { $($variant($params)),* }
        impl Request {
            pub fn command(&self) -> Result<Command, ProtocolError> {
                if self.protocol != VERSION { return Err(ProtocolError::Invalid("unsupported protocol version".into())); }
                if self.request_id.is_empty() || self.request_id.len() > 128 || !self.request_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)) { return Err(ProtocolError::Invalid("request_id must contain 1..128 ASCII letters, digits, . _ : or -".into())); }
                match self.method.as_str() {
                    $($method => serde_json::from_value(self.params.clone()).map(Command::$variant).map_err(|e|ProtocolError::Invalid(e.to_string())),)*
                    _ => Err(ProtocolError::Invalid("unknown method".into())),
                }
            }
        }
        #[cfg(test)]
        fn swift_commands() -> String {
            let mut s = String::from("public enum Command: Equatable, Sendable {\n");
            $(s.push_str(&format!("    case {}({})\n", stringify!($variant), stringify!($params)));)*
            s.push_str("}\n\npublic extension Request {\n    func command() throws -> Command {\n        guard self.protocol == protocolVersion else { throw FramingError.unsupportedVersion }\n        guard !request_id.isEmpty && request_id.utf8.count <= 128 && request_id.utf8.allSatisfy({ (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) || [46, 95, 58, 45].contains($0) }) else { throw FramingError.invalidFrame }\n        guard case .object(let fields) = params else { throw FramingError.invalidFrame }\n        let data = try JSONEncoder().encode(params)\n        let decoder = JSONDecoder()\n        switch method {\n");
            $(s.push_str(&format!("        case \"{}\":\n            guard Set(fields.keys).isSubset(of: Set<String>({})) else {{ throw FramingError.invalidFrame }}\n            return .{}(try decoder.decode({}.self, from: data))\n", $method, serde_json::to_string($params::swift_keys()).unwrap(), stringify!($variant), stringify!($params)));)*
            s.push_str("        default: throw FramingError.invalidFrame\n        }\n    }\n}\n\n"); s
        }
    }
}
commands! {
    MessageSubmit(SubmitParams) => "message.submit", Cancel(CancelParams) => "run.cancel",
    ApprovalDecide(ApprovalParams) => "approval.decide", Snapshot(EmptyParams) => "snapshot",
    Inspect(InspectParams) => "inspect", Shutdown(EmptyParams) => "shutdown",
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    Invalid(String),
    TooLarge,
    Truncated,
}
impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Invalid(e) => write!(f, "invalid protocol frame: {e}"),
            Self::TooLarge => write!(f, "protocol frame exceeds {MAX_FRAME_BYTES} bytes"),
            Self::Truncated => write!(f, "protocol frame ended without newline"),
        }
    }
}
impl std::error::Error for ProtocolError {}
impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Bounded incremental framing: never grows with attacker-controlled line size.
/// A framing error is fatal to this stream; the caller must close the worker.
pub fn read_request(reader: &mut impl BufRead) -> Result<Option<Request>, ProtocolError> {
    let Some(bytes) = read_frame(reader)? else {
        return Ok(None);
    };
    let request: Request = serde_json::from_slice(&bytes).map_err(|e| ProtocolError::Invalid(e.to_string()))?;
    request.command()?;
    Ok(Some(request))
}
pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, ProtocolError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if frame.is_empty() { Ok(None) } else { Err(ProtocolError::Truncated) };
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let count = newline.map_or(available.len(), |i| i + 1);
        if frame.len().saturating_add(count) > MAX_FRAME_BYTES {
            return Err(ProtocolError::TooLarge);
        }
        frame.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            frame.pop();
            return Ok(Some(frame));
        }
    }
}
pub fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<(), ProtocolError> {
    let bytes = serde_json::to_vec(value).map_err(|e| ProtocolError::Invalid(e.to_string()))?;
    if bytes.len().saturating_add(1) > MAX_FRAME_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
fn swift_schema() -> String {
    let mut s = format!(
        "// Generated by Rust protocol::generated_swift(); do not edit.\nimport Foundation\n\npublic let protocolVersion: UInt32 = {VERSION}\npublic let maxFrameBytes = {MAX_FRAME_BYTES}\n\n"
    );
    for definition in [
        Request::swift(),
        Event::swift(),
        SubmitParams::swift(),
        CancelParams::swift(),
        ApprovalParams::swift(),
        InspectParams::swift(),
        EmptyParams::swift(),
    ] {
        s.push_str(&definition);
    }
    s.push_str(&swift_commands());
    s.push_str(&PendingApprovalPayload::swift());
    s.push_str(&ToolFlightPayload::swift());
    s.push_str(&swift_payloads());
    s.push_str(include_str!("../apps/macos/RustyKit/JSONValue.swift.template"));
    s
}
#[cfg(test)]
pub fn generated_swift() -> String {
    format!("{}\npublic let protocolSchemaHash = \"{}\"\n", swift_schema(), computed_schema_hash())
}
pub fn schema_hash() -> String {
    include_str!("protocol_schema_hash.txt").trim().to_owned()
}
#[cfg(test)]
fn computed_schema_hash() -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, swift_schema().as_bytes());
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
#[cfg(test)]
pub fn fixture_frames() -> Vec<String> {
    use serde_json::json;
    let values = vec![
        json!({"protocol":VERSION,"instance_id":"fixture-host-1","seq":1,"kind":"hello","payload":{"protocol":VERSION,"schema_hash":computed_schema_hash(),"worker_instance_id":"fixture-host-1","inspect_only":false,"phase":"starting","protocol_capabilities":["message.submit","run.cancel","snapshot","approval.decide","shutdown"]}}),
        json!({"protocol":VERSION,"request_id":"submit-1","method":"message.submit","params":{"text":"Fix café 🦀\nthen test"}}),
        json!({"protocol":VERSION,"request_id":"cancel-1","method":"run.cancel","params":{"run_id":"run-1"}}),
        json!({"protocol":VERSION,"request_id":"approve-1","method":"approval.decide","params":{"approval_id":"approval-1","allowed":true,"operation_digest":"sha256:fixture-operation","typed":"yes"}}),
        json!({"protocol":VERSION,"request_id":"snapshot-1","method":"snapshot","params":{}}),
        json!({"protocol":VERSION,"request_id":"inspect-1","method":"inspect","params":{"operation":"file.read","arguments":{"path":"src/main.rs","start_line":1,"limit":5000}}}),
        json!({"protocol":VERSION,"instance_id":"fixture-host-1","seq":2,"kind":"approval.requested","payload":{"approval_id":"approval-1","operation_digest":"sha256:fixture-operation","operation":{"name":"bash","arguments":{"command":"git clean -fdx"},"requester":"lead"},"expires_at":1792275000u64,"typed_phrase":"yes","sensitive":true}}),
        json!({"protocol":VERSION,"instance_id":"fixture-host-1","seq":3,"kind":"session.snapshot","payload":{"worker_instance_id":"fixture-host-1","snapshot_watermark":2,"session_id":"fixture-session","run_id":null,"phase":"uncertain","live_text":"","live_reasoning":"","live_truncated":false,"last_tool":null,"pending_approval":null,"history":{"messages":[],"truncated":false},"cost_usd":null,"receipts":{},"receipts_truncated":false,"uncertain_run_count":1,"uncertain_runs":[{"request_id":"submit-1","run_id":"run-1"}],"uncertain_runs_truncated":false,"uncertain_tools":[],"uncertain_tool_count":0,"uncertain_tools_truncated":false,"in_flight_tools":[],"in_flight_tools_truncated":false}}),
        json!({"protocol":VERSION,"instance_id":"fixture-host-1","seq":4,"kind":"future.optional.notice","payload":{"extension":{"diagnostic":true}}}),
    ];
    values.into_iter().map(|v| serde_json::to_string(&v).expect("fixture serialization")).collect()
}
/// Additive envelope fields and missing optional parameters remain compatible.
/// Unlike canonical fixtures, ignored extension fields need not re-encode.
#[cfg(test)]
pub fn compatibility_frames() -> Vec<String> {
    use serde_json::json;
    [
        json!({"protocol":VERSION,"instance_id":"fixture-host-1","seq":4,"kind":"future.optional.notice","payload":{"diagnostic":true},"future_optional_metadata":{"revision":2}}),
        json!({"protocol":VERSION,"request_id":"optional-omitted","method":"approval.decide","params":{"approval_id":"a","operation_digest":"d","allowed":false},"future_optional_metadata":null}),
        json!({"protocol":VERSION,"request_id":"optional-null","method":"approval.decide","params":{"approval_id":"a","operation_digest":"d","allowed":false,"typed":null}}),
    ].into_iter().map(|v|v.to_string()).collect()
}
#[cfg(test)]
pub fn write_generated_artifacts(root: &Path) -> io::Result<()> {
    std::fs::write(root.join("src/protocol_schema_hash.txt"), computed_schema_hash() + "\n")?;
    std::fs::write(root.join("apps/macos/RustyKit/Sources/RustyProtocol/Protocol+Generated.swift"), generated_swift())?;
    std::fs::write(
        root.join("apps/macos/RustyKit/Tests/RustyProtocolTests/Fixtures/protocol.ndjson"),
        fixture_frames().join("\n") + "\n",
    )?;
    std::fs::write(
        root.join("apps/macos/RustyKit/Tests/RustyProtocolTests/Fixtures/forward-compat.ndjson"),
        compatibility_frames().join("\n") + "\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};
    #[test]
    #[ignore = "explicit developer artifact generation only"]
    fn generate_artifacts() {
        write_generated_artifacts(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    }
    #[test]
    fn generated_types_and_fixtures_are_current() {
        assert_eq!(schema_hash(), computed_schema_hash(), "run cargo xtask protocol-fixtures");
        assert_eq!(
            generated_swift(),
            include_str!("../apps/macos/RustyKit/Sources/RustyProtocol/Protocol+Generated.swift")
        );
        assert_eq!(
            fixture_frames().join("\n") + "\n",
            include_str!("../apps/macos/RustyKit/Tests/RustyProtocolTests/Fixtures/protocol.ndjson")
        );
    }
    #[test]
    fn additive_envelopes_and_optional_fields_decode() {
        assert_eq!(
            compatibility_frames().join("\n") + "\n",
            include_str!("../apps/macos/RustyKit/Tests/RustyProtocolTests/Fixtures/forward-compat.ndjson")
        );
        let frames = compatibility_frames();
        let event: Event = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(event.kind, "future.optional.notice");
        for line in &frames[1..] {
            let request = read_request(&mut Cursor::new(line.clone() + "\n")).unwrap().unwrap();
            match request.command().unwrap() {
                Command::ApprovalDecide(params) => assert_eq!(params.typed, None),
                _ => panic!("expected approval"),
            }
        }
    }
    #[test]
    fn canonical_fixtures_round_trip() {
        for line in fixture_frames() {
            let value: Value = serde_json::from_str(&line).unwrap();
            let encoded = if value.get("method").is_some() {
                let request: Request = serde_json::from_value(value.clone()).unwrap();
                request.command().unwrap();
                serde_json::to_value(request).unwrap()
            } else {
                {
                    let event: Event = serde_json::from_value(value.clone()).unwrap();
                    event.validate_payload().unwrap();
                    serde_json::to_value(event).unwrap()
                }
            };
            assert_eq!(encoded, value);
        }
    }
    #[test]
    fn recorded_worker_journey_matches_the_generated_event_contract() {
        let recorded = include_str!("../apps/macos/RustyKit/Tests/RustyProtocolTests/Fixtures/recorded-worker.ndjson");
        let mut last = 0;
        let mut kinds = std::collections::BTreeSet::new();
        for line in recorded.lines() {
            let event: Event = serde_json::from_str(line).unwrap();
            event.validate_payload().unwrap();
            assert!(event.seq > last);
            last = event.seq;
            kinds.insert(event.kind.clone());
            if event.kind == "hello" {
                assert_eq!(event.seq, 1);
                assert_eq!(event.payload["schema_hash"], schema_hash());
            }
        }
        for kind in [
            "session.ready",
            "session.snapshot",
            "approval.requested",
            "approval.resolved",
            "tool.proposed",
            "tool.started",
            "tool.completed",
            "message.delta",
            "message.completed",
            "run.usage",
            "run.completed",
        ] {
            assert!(kinds.contains(kind), "recorded journey omitted {kind}");
        }
    }

    #[test]
    fn streaming_payloads_reject_missing_or_wrongly_typed_fields() {
        for (kind, payload) in [
            ("message.delta", serde_json::json!({"text": 42})),
            ("reasoning.delta", serde_json::json!({})),
            ("run.usage", serde_json::json!({"prompt_tokens": "many"})),
            ("protocol.error", serde_json::json!({"error": null})),
        ] {
            let event = Event { protocol: VERSION, instance_id: "test".into(), seq: 2, kind: kind.into(), payload };
            assert!(event.validate_payload().is_err(), "invalid {kind} payload must not become an untyped event");
        }
        let event = Event {
            protocol: VERSION,
            instance_id: "test".into(),
            seq: 3,
            kind: "message.delta".into(),
            payload: serde_json::json!({"text":"café 🦀", "future_metadata":true}),
        };
        event.validate_payload().unwrap();
    }
    #[test]
    fn incremental_and_partial_frames() {
        let frame = fixture_frames()[1].clone() + "\n";
        let mut reader = BufReader::with_capacity(3, Cursor::new(frame));
        assert!(read_request(&mut reader).unwrap().is_some());
        assert!(read_request(&mut reader).unwrap().is_none());
        assert!(matches!(read_frame(&mut Cursor::new(b"{}")), Err(ProtocolError::Truncated)));
        assert!(matches!(read_frame(&mut Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 1])), Err(ProtocolError::TooLarge)));
    }
    #[test]
    fn invalid_requests_fail_closed() {
        for value in [
            serde_json::json!({"protocol":2,"request_id":"x","method":"snapshot","params":{}}),
            serde_json::json!({"protocol":1,"request_id":"x","method":"unknown","params":{}}),
            serde_json::json!({"protocol":1,"request_id":"x","method":"run.cancel","params":{}}),
            serde_json::json!({"protocol":1,"request_id":"x","method":"approval.decide","params":{"allowed":true}}),
            serde_json::json!({"protocol":1,"request_id":"x","method":"snapshot","params":{"unexpected":true}}),
            serde_json::json!({"protocol":1,"request_id":"x","method":"approval.decide","params":{"approval_id":"a","operation_digest":"d","allowed":true,"always":true}}),
        ] {
            assert!(read_request(&mut Cursor::new(value.to_string() + "\n")).is_err());
        }
        assert!(read_request(&mut Cursor::new(b"{broken}\n")).is_err());
    }
    #[test]
    fn every_command_rejects_unknown_parameter_keys() {
        for (method, mut params) in [
            ("message.submit", serde_json::json!({"text":"hello"})),
            ("run.cancel", serde_json::json!({"run_id":"run"})),
            ("approval.decide", serde_json::json!({"approval_id":"a","operation_digest":"d","allowed":true})),
            ("snapshot", serde_json::json!({})),
            ("inspect", serde_json::json!({"operation":"tree.snapshot","arguments":{}})),
            ("shutdown", serde_json::json!({})),
        ] {
            let mut request = Request {
                protocol: VERSION,
                request_id: "key-test".into(),
                method: method.into(),
                params: params.clone(),
            };
            assert!(request.command().is_ok(), "valid {method}");
            params["unexpected_authority"] = serde_json::json!(true);
            request.params = params;
            assert!(request.command().is_err(), "extra field in {method}");
        }
    }
    #[test]
    fn request_ids_use_the_pinned_printable_charset() {
        let mut request = Request {
            protocol: VERSION,
            request_id: "AZaz09._:-".into(),
            method: "snapshot".into(),
            params: serde_json::json!({}),
        };
        assert!(request.command().is_ok());
        request.request_id = "x".repeat(128);
        assert!(request.command().is_ok());
        for id in [
            String::new(),
            "x".repeat(129),
            "line\nbreak".into(),
            "\0".into(),
            "café".into(),
            "a/b".into(),
            "a b".into(),
        ] {
            request.request_id = id;
            assert!(request.command().is_err());
        }
    }
    #[test]
    fn writer_checks_size_before_writing() {
        let mut out = Vec::new();
        assert!(write_frame(&mut out, &"x".repeat(MAX_FRAME_BYTES)).is_err());
        assert!(out.is_empty());
        write_frame(&mut out, &serde_json::json!({"ok":true})).unwrap();
        assert_eq!(out, b"{\"ok\":true}\n");
    }
}
