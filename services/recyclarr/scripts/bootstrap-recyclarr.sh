#!/usr/bin/env bash
set -euo pipefail

STACK_DIR="/opt/media"
ENV_FILE="$STACK_DIR/.env"
COMPOSE_FILE="$STACK_DIR/docker-compose.yml"

if [[ -f "$ENV_FILE" ]]; then
  set -a
  . "$ENV_FILE"
  set +a
fi

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-media}"
docker_compose() {
  docker compose -p "$COMPOSE_PROJECT_NAME" --project-directory "$STACK_DIR" --env-file "$ENV_FILE" -f "$COMPOSE_FILE" "$@"
}

python3 <<'PY'
import os
import time
import xml.etree.ElementTree as ET
from pathlib import Path

config_root = Path(os.environ.get("CONFIG_PATH") or "/opt/media/config")
recyclarr_dir = config_root / "recyclarr"
recyclarr_dir.mkdir(parents=True, exist_ok=True)
config_path = recyclarr_dir / "recyclarr.yml"


def read_api_key(path: Path) -> str:
    for _ in range(180):
        if path.exists():
            root = ET.parse(path).getroot()
            key = (root.findtext("ApiKey") or "").strip()
            if key:
                return key
        time.sleep(2)
    raise RuntimeError(f"missing API key in {path}")


sonarr_key = read_api_key(config_root / "sonarr" / "config.xml")
radarr_key = read_api_key(config_root / "radarr" / "config.xml")
sonarr_url = os.environ.get("SONARR_INTERNAL_URL", "http://sonarr:8989")
radarr_url = os.environ.get("RADARR_INTERNAL_URL", "http://radarr:7878")

config_path.write_text(
    f"""# yaml-language-server: $schema=https://raw.githubusercontent.com/recyclarr/recyclarr/master/schemas/config-schema.json
sonarr:
  tv:
    base_url: {sonarr_url}
    api_key: {sonarr_key}
    delete_old_custom_formats: true
    quality_definition:
      type: series
    quality_profiles:
      - trash_id: 72dae194fc92bf828f32cde7744e51a1
        reset_unmatched_scores:
          enabled: true
    custom_formats:
      - trash_ids:
          - 69aa1e159f97d860440b04cd6d590c4f # Language: Not English
        assign_scores_to:
          - trash_id: 72dae194fc92bf828f32cde7744e51a1
            score: -10000
radarr:
  movies:
    base_url: {radarr_url}
    api_key: {radarr_key}
    delete_old_custom_formats: true
    quality_definition:
      type: movie
    quality_profiles:
      - trash_id: d1d67249d3890e49bc12e275d989a7e9
        reset_unmatched_scores:
          enabled: true
    custom_formats:
      - trash_ids:
          - 0dc8aec3bd1c47cd6c40c46ecd27e846 # Language: Not English
        assign_scores_to:
          - trash_id: d1d67249d3890e49bc12e275d989a7e9
            score: -10000
""",
    encoding="utf-8",
)

loop_path = recyclarr_dir / "recyclarr-loop.sh"
loop_path.write_text(
    """#!/usr/bin/env sh
set -eu
while true; do
  /app/recyclarr/recyclarr sync -c /config/recyclarr.yml || true
  sleep ${RECYCLARR_SYNC_INTERVAL_SECONDS:-86400}
done
""",
    encoding="utf-8",
)
loop_path.chmod(0o755)
PY

docker_compose up -d recyclarr
for attempt in $(seq 1 12); do
  if docker_compose exec -T recyclarr sh -lc '/app/recyclarr/recyclarr sync -c /config/recyclarr.yml'; then
    break
  fi
  if [[ "$attempt" -eq 12 ]]; then
    exit 1
  fi
  echo "[recyclarr] sync attempt ${attempt} failed; retrying"
  sleep 10
done
