# TCP Spammer

This tool allows measuring link performance by torturing it with huge numbers of
new connections.

There are tools that allow measuring link throughput in terms of throughput in
bytes, like iperf. However, if the link contains something *smart*, not just a
dumb wire (eg. a stateful firewall, router with NAT), there's a big difference
between making a single TCP connection and sending a lot of data through it
and creating a lot of new connections, each transferring only little bit of
data. I was interested in the latter.

There probably are tools for that too, but I wanted to play, so I wrote this.

## Maturity, maintenance, contributing...

It gets the job done. It isn't something you would want in production, but the
purpose is obviously not production, but testing and it's fine for that.

As it gets done what I needed, I don't expect to be extending or maintaining it
much.

That being said, if you find it useful, need a little feature or want to take it
over, feel free to open an issue or a pull request. I'll try to respond and
discuss the needs.

## Working principle

There's a server and there's a client. The client connects to the server, sends
a bit of data, closes its half of connection, then the server sends bit of data
and closes the other half of the connection. That's it.

However, all this can be done massively in parallel.

The server has only one mode, where it simply listens and answers connections as
fast as it can.

The client has two modes. One with manual setting of rate. Rate is how many new
connections are made per second ‒ they are created and latency statistics of the
whole connection turn around is measured (minimum, maximum, mean and 90th
percentile).

In the second mode the client tries to find at which rate the link gets
saturated. It increases the rate in steps and tries to detect significant rise
in the latency median.

All the parameters are configurable on command line.

## Performance

When dry-testing, the tool was able to saturate a gigabit ethernet link with 3kB
payloads on ordinary commodity hardware. It was around 25k connections per
second. This is expected to be enough to flood any kind of ordinary *smart*
device.

However, to do so, it is necessary to tweak some OS settings. The problem is the
existence of too many parallel and new connections.

* Raise the limit of number of file descriptors, eg. `ulimit -n 100000`, both on
  the server and client side.
* Allow as many ports usable on the client side as possible:
  `echo 1024 65535 > /proc/sys/net/ipv4/ip_local_port_range`.
* Allow reusing of ports in `TIME_WAIT` state:
  `echo 1 > /proc/sys/net/ipv4/tcp_tw_reuse`.
* Raise the maximum number of waiting TCP connections on the server side:
  `echo 20480 > /proc/sys/net/ipv4/tcp_max_syn_backlog`.

Even in these settings, weird things were sometimes happening, eg:
* Some connections were getting resetted.
* Sometimes, the OS still run out of available ports. Use of the `--cooldown`
  parameter might help a bit, or using a shorter `--length` of the test.
* Sometimes, *something* in the kernel switches and starts consuming a lot of
  CPU while dropping the performance significantly. I didn't manage to find out
  what exactly, but I believe it is related to running out of the available
  local ports on the client, or nearly so.

## Installation

Compile with Rust version 1.39 or newer (or nightly, if 1.39 is not released yet
😇).

```
cargo +nightly install --git https://github.com/vorner/tspam
```

Or simply run it:

```
cargo +nightly run --release -- --help
```

## License

Licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
