#!/usr/bin/env bash
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

RESOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$RESOURCE_DIR/gitea-runner.env"

if [[ -f "$ENV_FILE" ]]; then
  set -a
  # shellcheck source=/dev/null
  . "$ENV_FILE"
  set +a
fi

GITEA_RUNNER_ENABLED="${GITEA_RUNNER_ENABLED:-true}"
GITEA_RUNNER_ACT_VERSION="${GITEA_RUNNER_ACT_VERSION:-0.2.11}"
GITEA_RUNNER_REGISTRATION_RETRIES="${GITEA_RUNNER_REGISTRATION_RETRIES:-60}"
GITEA_RUNNER_REGISTRATION_RETRY_DELAY_SECONDS="${GITEA_RUNNER_REGISTRATION_RETRY_DELAY_SECONDS:-5}"

is_truthy() {
  local value="${1:-}"
  case "${value,,}" in
    1|true|yes|on) return 0 ;;
    *) return 1 ;;
  esac
}

if ! is_truthy "$GITEA_RUNNER_ENABLED"; then
  echo "gitea runner feature disabled"
  exit 0
fi

missing=()
for package in ca-certificates curl git jq openssh-client python3; do
  dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q 'install ok installed' || missing+=("$package")
done
if ((${#missing[@]} > 0)); then
  apt-get update
  apt-get install -y "${missing[@]}"
fi

install_docker() {
  if command -v docker >/dev/null 2>&1 && docker version >/dev/null 2>&1; then
    systemctl enable --now docker
    return 0
  fi

  . /etc/os-release
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL "https://download.docker.com/linux/${ID}/gpg" -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  cat > /etc/apt/sources.list.d/docker.list <<DOCKER_REPO

deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/${ID} ${VERSION_CODENAME} stable
DOCKER_REPO

  apt-get update
  if ! apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin; then
    apt-get install -y docker.io
  fi
  systemctl enable --now docker
}

ensure_act_runner_user() {
  if ! id act_runner >/dev/null 2>&1; then
    groupadd --system act_runner
    useradd --system --home /var/lib/act_runner --create-home --shell /usr/sbin/nologin --gid act_runner act_runner
  fi
  install -d -m 0750 -o act_runner -g act_runner /var/lib/act_runner
  if getent group docker >/dev/null 2>&1; then
    usermod -aG docker act_runner || true
  fi
}

install_act_runner_binary() {
  local arch url checksum_url tmp_bin expected
  case "$(uname -m)" in
    x86_64) arch="amd64" ;;
    aarch64|arm64) arch="arm64" ;;
    *)
      echo "unsupported architecture for act_runner: $(uname -m)"
      exit 1
      ;;
  esac

  url="https://dl.gitea.com/act_runner/${GITEA_RUNNER_ACT_VERSION}/act_runner-${GITEA_RUNNER_ACT_VERSION}-linux-${arch}"
  checksum_url="${url}.sha256"
  tmp_bin="$(mktemp)"

  curl -fsSL "$url" -o "$tmp_bin"
  if expected="$(curl -fsSL "$checksum_url" | awk '{print $1}' | head -n1)" && [[ -n "$expected" ]]; then
    echo "${expected}  ${tmp_bin}" | sha256sum -c - >/dev/null
  fi

  install -m 0755 "$tmp_bin" /usr/local/bin/act_runner
  rm -f "$tmp_bin"
}

instance_indexes() {
  local vars=() indices=() var idx
  while IFS= read -r var; do
    vars+=("$var")
  done < <(compgen -A variable GITEA_RUNNER_INSTANCE_NAME_ || true)

  for var in "${vars[@]}"; do
    idx="${var##*_}"
    [[ -n "$idx" ]] && indices+=("$idx")
  done

  if ((${#indices[@]} == 0)); then
    indices=(0)
  fi

  printf '%s\n' "${indices[@]}" | sort -n -u
}

endpoint_for_scope() {
  local base_url="$1"
  local scope="$2"
  local repo="$3"
  local org="$4"
  case "$scope" in
    repo)
      if [[ "$repo" != */* ]]; then
        echo "repo scope requires owner/repo, got: $repo" >&2
        return 1
      fi
      local owner="${repo%%/*}"
      local repo_name="${repo#*/}"
      printf '%s/api/v1/repos/%s/%s/actions/runners/registration-token\n' "${base_url%/}" "$owner" "$repo_name"
      ;;
    org)
      if [[ -z "$org" ]]; then
        echo "org scope requires org name" >&2
        return 1
      fi
      printf '%s/api/v1/orgs/%s/actions/runners/registration-token\n' "${base_url%/}" "$org"
      ;;
    instance)
      printf '%s/api/v1/admin/actions/runners/registration-token\n' "${base_url%/}"
      ;;
    *)
      echo "unsupported runner scope: $scope" >&2
      return 1
      ;;
  esac
}

request_registration_token() {
  local endpoint="$1"
  local admin_user="$2"
  local admin_password="$3"
  local headers_file body_file code token

  headers_file="$(mktemp)"
  body_file="$(mktemp)"
  code="$(curl -sS -D "$headers_file" -o "$body_file" -w '%{http_code}' -u "$admin_user:$admin_password" -X POST "$endpoint")"

  if [[ "$code" != "200" ]]; then
    echo "failed to request runner registration token (${code}) from ${endpoint}" >&2
    cat "$body_file" >&2 || true
    rm -f "$headers_file" "$body_file"
    return 1
  fi

  token="$(awk 'tolower($1) == "token:" {gsub("\r", "", $2); print $2; exit}' "$headers_file")"
  if [[ -z "$token" ]]; then
    token="$(python3 - "$body_file" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
try:
    payload = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    print("")
    raise SystemExit(0)
print(str(payload.get("token") or payload.get("Token") or ""))
PY
)"
  fi

  rm -f "$headers_file" "$body_file"
  if [[ -z "$token" ]]; then
    echo "gitea returned empty registration token for ${endpoint}" >&2
    return 1
  fi
  printf '%s\n' "$token"
}

wait_for_api() {
  local base_url="$1"
  local deadline=$((SECONDS + 300))
  while ((SECONDS < deadline)); do
    if curl -fsS "${base_url%/}/api/v1/version" >/tmp/vmctl-gitea-runner-version.json 2>/dev/null; then
      return 0
    fi
    sleep 2
  done
  return 1
}

install_docker
ensure_act_runner_user
install_act_runner_binary

install -d -m 0750 -o root -g act_runner /etc/act_runner

cat > /etc/systemd/system/act_runner@.service <<'EOF_UNIT'
[Unit]
Description=Gitea Actions Runner (%i)
Documentation=https://gitea.com/gitea/act_runner
After=network-online.target docker.service
Wants=network-online.target

[Service]
Type=simple
User=act_runner
Group=act_runner
WorkingDirectory=/var/lib/act_runner/%i
ExecStart=/usr/local/bin/act_runner daemon --config /etc/act_runner/%i.yaml
ExecReload=/bin/kill -s HUP $MAINPID
Restart=always
RestartSec=5
TimeoutSec=0

[Install]
WantedBy=multi-user.target
EOF_UNIT
systemctl daemon-reload

while IFS= read -r idx; do
  name_var="GITEA_RUNNER_INSTANCE_NAME_${idx}"
  base_url_var="GITEA_RUNNER_INSTANCE_BASE_URL_${idx}"
  scope_var="GITEA_RUNNER_INSTANCE_SCOPE_${idx}"
  repo_var="GITEA_RUNNER_INSTANCE_REPO_${idx}"
  org_var="GITEA_RUNNER_INSTANCE_ORG_${idx}"
  runner_name_var="GITEA_RUNNER_INSTANCE_RUNNER_NAME_${idx}"
  labels_var="GITEA_RUNNER_INSTANCE_LABELS_${idx}"
  capacity_var="GITEA_RUNNER_INSTANCE_CAPACITY_${idx}"
  admin_user_var="GITEA_RUNNER_INSTANCE_ADMIN_USER_${idx}"
  admin_password_var="GITEA_RUNNER_INSTANCE_ADMIN_PASSWORD_${idx}"
  ephemeral_var="GITEA_RUNNER_INSTANCE_EPHEMERAL_${idx}"

  instance_name="${!name_var:-gitea-${idx}}"
  base_url="${!base_url_var:-http://gitea:3000/}"
  scope="${!scope_var:-repo}"
  repo="${!repo_var:-admin/vmctl}"
  org="${!org_var:-}"
  runner_name="${!runner_name_var:-${VMCTL_RESOURCE_NAME:-gitea-runner}-${idx}}"
  labels="${!labels_var:-vmctl:docker://rust:1.78-bookworm}"
  capacity="${!capacity_var:-1}"
  admin_user="${!admin_user_var:-admin}"
  admin_password="${!admin_password_var:-changeme}"
  ephemeral_raw="${!ephemeral_var:-false}"

  instance_slug="$(printf '%s' "$instance_name" | tr '[:upper:]' '[:lower:]' | tr -cs 'a-z0-9' '-')"
  instance_slug="${instance_slug#-}"
  instance_slug="${instance_slug%-}"
  if [[ -z "$instance_slug" ]]; then
    instance_slug="gitea-${idx}"
  fi

  instance_dir="/var/lib/act_runner/${instance_slug}"
  config_file="/etc/act_runner/${instance_slug}.yaml"
  install -d -m 0750 -o act_runner -g act_runner "$instance_dir"

  cat > "$config_file" <<EOF_CFG
log:
  level: info
runner:
  file: ${instance_dir}/.runner
  capacity: ${capacity}
  labels:
EOF_CFG
  IFS=',' read -r -a label_items <<<"$labels"
  for label in "${label_items[@]}"; do
    label="$(echo "$label" | xargs)"
    [[ -n "$label" ]] || continue
    printf '    - %s\n' "$label" >>"$config_file"
  done
  if ! grep -q '^    - ' "$config_file"; then
    echo "    - vmctl:docker://rust:1.78-bookworm" >>"$config_file"
  fi
  cat >> "$config_file" <<'EOF_CFG'
container:
  privileged: false
  valid_volumes: []
  docker_host: "-"
EOF_CFG
  chown root:act_runner "$config_file"
  chmod 0640 "$config_file"

  if [[ ! -s "${instance_dir}/.runner" ]]; then
    if ! wait_for_api "$base_url"; then
      echo "gitea api not reachable for runner registration: ${base_url}" >&2
      exit 1
    fi

    endpoint="$(endpoint_for_scope "$base_url" "$scope" "$repo" "$org")"

    retries="$GITEA_RUNNER_REGISTRATION_RETRIES"
    delay="$GITEA_RUNNER_REGISTRATION_RETRY_DELAY_SECONDS"
    registration_token=""

    for ((attempt = 1; attempt <= retries; attempt++)); do
      if registration_token="$(request_registration_token "$endpoint" "$admin_user" "$admin_password")"; then
        break
      fi
      if ((attempt == retries)); then
        echo "runner registration token request failed after ${retries} attempts" >&2
        exit 1
      fi
      sleep "$delay"
    done

    register_args=(
      --config "$config_file"
      register
      --no-interactive
      --instance "$base_url"
      --token "$registration_token"
      --name "$runner_name"
      --labels "$labels"
    )
    if is_truthy "$ephemeral_raw"; then
      register_args+=(--ephemeral)
    fi

    runuser -u act_runner -- /usr/local/bin/act_runner "${register_args[@]}"
    chmod 0600 "${instance_dir}/.runner"
    chown act_runner:act_runner "${instance_dir}/.runner"
  fi

  systemctl enable --now "act_runner@${instance_slug}.service"
  systemctl restart "act_runner@${instance_slug}.service"
done < <(instance_indexes)

echo "gitea runner bootstrap complete"
