# VPN Verification

Use these checks after `vmctl apply` to confirm the torrent stack is still inside Mullvad.

## Container Checks

```bash
docker compose exec gluetun curl -fsS ifconfig.me
docker compose exec qbittorrent-vpn curl -fsS ifconfig.me
```

Expected:
- `gluetun` returns the Mullvad exit IP
- `qbittorrent-vpn` returns the same IP
- neither command should return the host WAN IP

## qBittorrent Checks

```bash
docker compose exec qbittorrent-vpn curl -fsS http://127.0.0.1:8080/api/v2/app/version
docker compose logs gluetun
docker compose logs qbittorrent-vpn
```

Confirm:
- `qbittorrent-vpn` is using `network_mode: service:gluetun`
- gluetun reports a connected WireGuard tunnel
- qBittorrent Web UI is reachable only through the VPN network namespace

## Leak Checks

- Compare the IP from `gluetun` with the host IP
- Check that torrent peers only see the Mullvad IP
- Re-run the commands after a fresh `vmctl apply`
