# espnow-reliable

Reliable, ordered message delivery between two ESP-NOW peers.

ESP-NOW frames can be lost, and resent frames can arrive twice. This crate numbers each message, resends it until the peer acknowledges it, and hands received messages to your application exactly once and in the order they were sent. It also notices when the peer restarts, so your application can start over instead of mixing old and new state.

The crate is a `no_std` state machine. It does not allocate, does no I/O and keeps no clock, so it works with esp-radio, ESP-IDF, or any other driver that can send and receive datagrams. Both peers must use it.

## Usage

Keep one `Link` per peer MAC address and drive it from your radio loop:

1. Queue messages with `Link::send`.
2. Pass every frame received from that peer to `Link::handle`, which calls you back with each message that is ready.
3. Send whatever `Link::poll_transmit` returns, until it returns `None`, after each of the calls above and whenever the time from `Link::poll_timeout` passes.
4. Check `Link::poll_event` to learn when the link connects, when the peer restarts, and when the link fails.

The crate documentation has a complete example.

Each `Link` needs a random session id from a hardware random number generator, different every time a link is created, including across reboots. That is how the peer tells a restart apart from a continuing connection.

## Sizing

`Link<WINDOW, N>` takes two const parameters. `WINDOW` is how many messages can wait for acknowledgement at once (a power of two up to 32, default 8). `N` is the largest frame the radio sends. It defaults to 1470 bytes, the ESP-NOW v2 limit. Use `MAX_DATA_LEN_V1` (250) if any peer runs ESP-NOW v1. Each message carries a 17-byte header, so the largest message is `N - 17` bytes.

A link holds two buffers of `N` bytes for each window slot, which is about 24 KB with the defaults. Keep it in a static or on the heap rather than on a task's stack.

## Pitfalls

`send` returns `SendError::Full` when the window is full. Wait for acknowledgements and try again.

Messages queued before a handshake are sent once it's finished. Handshake frames repeat every `hello_interval` until the peer answers, with no time limit.

`Event::Reset` means the peer restarted. Messages the peer didn't ACK are discarded, and the link automatically handshakes again. Throw away anything your application built on the old connection.

`Event::Failed` means a message went unacknowledged after `max_retransmits` resends. This closes the link, you will need to open a new one.

After a peer restarts, frames from its previous session are ignored for `max_frame_age` (100 ms by default). The link assumes no frame takes longer than that to arrive. ESP-NOW sends a frame within a few milliseconds or drops it, so the default leaves a wide margin.

Broadcast and peer discovery are out of scope.

## License

Licensed under either the [MIT license](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), whichever you prefer.
