# Fleet networking

Claude Code loads this file when it works in this directory. It holds the rules for the code here, moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line pointer to it.

## Fleet networking: what an agent sets up, and what it doesn't

- **Joining the fleet needs no Tailscale Serve.** Each machine's fleet listener (`fleet.listener.enabled true`, port 8766) binds its own tailnet address directly. Cards, work and radio travel on it with no tailnet settings changed. A listener behind Serve would make every caller look like the machine itself.
- **Serve is only for opening a machine's viewer from another device.** The daemon stays on loopback. Default to plain HTTP, tailnet-only: `tailscale serve --bg --http=<port> http://127.0.0.1:<serve.port>`. It needs no tailnet-wide setting. Propose `--https` only if the operator asks: it needs the tailnet's HTTPS-certificates setting, which writes the machine name to a public log.
- **Never propose or accept Funnel.** Tailscale's consent page offers it pre-ticked. It exposes the daemon to the internet, and darkmux reads are tailnet-open by design.
- Full recipe: `docs/guide/always-on-hub.html#viewer`, and `docs/guide/fleet.html` Part 2.
