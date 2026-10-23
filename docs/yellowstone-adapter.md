# Yellowstone gRPC adapter (Phases 5 and 8)

`src/monitor/geyser.rs` contains the whole supervision logic of the stream
monitor (reconnection with exponential backoff and jitter, idle watchdog, health
reporting, graceful shutdown, stale-update filtering) behind two small traits.
The network transport itself is **not** part of this crate yet, because a
Yellowstone client needs `tonic`, `prost` and the generated protobufs, none of
which are in `Cargo.lock`, and the dependency set may only change where crates.io
is reachable. This document specifies the adapter that closes the gap.

## Contract

```rust
pub trait StreamTransport: Send {
    type Stream: EventStream;
    fn connect(&mut self, request: &SubscriptionRequest)
        -> impl Future<Output = Result<Self::Stream, MonitorError>> + Send;
}

pub trait EventStream: Send {
    fn next_update(&mut self)
        -> impl Future<Output = Result<Option<StreamUpdate>, MonitorError>> + Send;
}
```

- `connect` dials, authenticates and sends the subscription. It is called again
  after every failure, so it must not keep state that survives a dead channel.
- `next_update` must be **cancel safe**: the monitor drops it when the shutdown
  signal or the idle watchdog wins the `select!`. `tonic::Streaming::message` is.
- `Ok(None)` means the server closed the stream; the monitor reconnects.
- Return `MonitorError::Connection { kind }` with the right `StreamErrorKind`.
  `Unauthenticated` and `InvalidRequest` are **not** retried (a bad token never
  becomes good); everything else is.

## Mapping from `SubscribeUpdate`

| Yellowstone update | `StreamUpdate` |
|--------------------|----------------|
| `UpdateOneof::Account` | `Account(AccountUpdate { slot, write_version, pubkey, owner, lamports, data })`, with `data: Bytes::from(vec)` (moves the buffer, no copy) |
| `UpdateOneof::Transaction` (inner/outer instruction of the Pump program) | one `Instruction(InstructionUpdate { slot, program_id, data })` per matching instruction |
| `UpdateOneof::Slot` | `Slot(slot)` |
| `UpdateOneof::Ping` / `Pong` | `Heartbeat` (and answer pings with a `SubscribeRequestPing`) |

Pubkeys arrive as 32-byte vectors: use `Pubkey::try_from(slice)` and map a wrong
length to `MonitorError::MalformedUpdate`, never to a panic.

## Subscription

`SubscriptionRequest` carries the watched accounts (the bonding curve and, when
needed, the wallet's token account), the program whose instructions to stream and
the commitment. Build the Yellowstone request from it:

- `accounts`: one filter with `account: [bonding_curve, ...]`, no `owner`/`memcmp` filters.
- `transactions`: `account_include: [bonding_curve]`, `vote: false`, `failed: false`.
- `slots`: enabled (cheap, and gives the monitor a second liveness signal).
- `commitment`: `Processed` for the 1-10 s exit window, `Confirmed` if you prefer
  fewer reorgs over latency.
- `ping`: `Some(SubscribeRequestPing { id: 1 })` every few seconds.

## Authentication and secrets

Send the `x-token` header through the client builder. Keep the token in a
`SecretString` (zeroized on drop) and never log it or put it in an error; the
`Debug` of the transport must redact it. Endpoints must be `https://` (TLS) except
loopback in tests.

## Wiring

```text
YellowstoneTransport --(GeyserMonitor)--> mpsc<StreamUpdate> --> ExitManager --> mpsc<SellOrder> --> run_sell_executor --> SellSubmitter
                                                                       ^                                      |
                                                                       +----------- mpsc<SellReport> ---------+
```

All channels are bounded; orders use `try_send` into a queue larger than the most
orders one plan can issue, so the manager can never block on the executor. Pass the
same `ShutdownSignal` to the monitor, the manager and the executor
(`shutdown_channel()`, `trigger_on_ctrl_c`).

## Testing the adapter

Reuse the `ScriptedTransport` pattern from `tests/phase5_tests.rs` for the monitor
and put the protobuf-to-`StreamUpdate` conversion in a pure function so it can be
unit-tested with hand-built `SubscribeUpdate` values, without a server.

## Status (Phase 8)

The adapter core is implemented in `src/network/yellowstone.rs`: the request and message
mapping above are the functions `build_wire_request` and `convert_message`, the transport
is `YellowstoneTransport`, the stream is `YellowstoneStream`. What remains is the
`tonic` binding, behind two traits.

```rust
pub trait YellowstoneLink: Send {
    type Session: YellowstoneSession;
    fn open(&mut self, settings: &YellowstoneSettings, request: &WireRequest)
        -> impl Future<Output = Result<Self::Session, MonitorError>> + Send;
}
pub trait YellowstoneSession: Send {
    fn next_message(&mut self) -> impl Future<Output = Result<Option<WireMessage>, MonitorError>> + Send;
    fn answer_ping(&mut self) -> impl Future<Output = Result<(), MonitorError>> + Send;
}
```

### Binding checklist (needs crates.io; the lockfile is regenerated by `cargo`, never by hand)

1. Add `tonic`, `prost` (if needed) and `yellowstone-grpc-proto` (or the `yellowstone-grpc-client` crate)
   to `[workspace.dependencies]`, pick versions that share one `tonic`, run `cargo update -w`, review and
   commit the `Cargo.lock` diff. `cargo audit` must stay clean.
2. `open`: build the channel from `settings.endpoint()` (TLS with the platform roots for `https`), send
   `settings.x_token()` as the `x-token` metadata (never log it), keep the connect deadline to the
   supervisor (`MonitorSettings::connect_timeout`), call `Geyser::Subscribe`, send the first
   `SubscribeRequest` built from the `WireRequest` and return the sink/stream pair as the session.
   Map failures with `grpc_failure(status.code() as i32)`; a transport error with no status is
   `StreamErrorKind::Unreachable` (or `Tls`).
3. `WireRequest` -> `SubscribeRequest`, one field at a time:
   `accounts["krypton"] = { account: wire.accounts, owner: [], filters: [] }`;
   if `transaction_account_include` is non-empty,
   `transactions["krypton"] = { vote: Some(false), failed: Some(false), account_include: ..., ..Default }`;
   `slots["krypton"] = { filter_by_commitment: Some(true), .. }` when `include_slots`;
   `commitment: Some(wire.commitment)`; `ping: Some(SubscribeRequestPing { id: PING_ID })`.
4. `next_message`: `stream.message().await` (cancel safe). `Ok(None)` -> `Ok(None)`; a `Status` ->
   `Err(grpc_failure(code))`. Convert each `SubscribeUpdate`:
   `Account` -> `WireMessage::Account` (a missing `account` info is a protocol violation:
   `Err(MonitorError::Connection { kind: Protocol })`); `Transaction` -> `WireMessage::Transaction`
   (`message.account_keys`, `meta.loaded_writable_addresses`, `meta.loaded_readonly_addresses`,
   `message.instructions`, and `meta.inner_instructions` flattened in order; a missing
   transaction/message/meta is a protocol violation); `Slot`, `Ping`, `Pong` -> the matching variants;
   anything else -> `WireMessage::Unsupported`. Move the `Vec<u8>` buffers; do not clone them.
5. `answer_ping`: `sink.send(SubscribeRequest { ping: Some(SubscribeRequestPing { id: PING_ID }), ..Default })`.
6. Raise the client's maximum decoding message size only as far as the largest account data you
   subscribe to needs (a bonding curve is 166 bytes).
7. Test the binding against a real endpoint (curve account, ping, forced drop) before relying on it;
   `tests/phase8_tests.rs` shows the scripted connections to mirror.

### Secrets

`YellowstoneSettings::new` validates the endpoint (`https`, loopback `http` only for tests, no
credentials, valid port) and the token. Its `Debug` prints the origin (`scheme://host:port`) and
whether a token is set; providers often put an access token in the URL path, so log
`settings.origin()`, never `settings.endpoint()`.
