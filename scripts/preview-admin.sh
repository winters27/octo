#!/usr/bin/env bash
set -euo pipefail

container_name=octo-ui-preview
image_name=octo:ui-preview

if [[ "${1:-}" == stop ]]; then
  docker stop "$container_name" >/dev/null 2>&1 || true
  echo "Stopped $container_name."
  exit 0
fi

preview_port="${1:-5277}"
preview_url="http://localhost:${preview_port}/admin/index.html"

if docker container inspect "$container_name" >/dev/null 2>&1; then
  docker rm -f "$container_name" >/dev/null
fi

echo "Building the current working tree as $image_name..."
docker build --load -t "$image_name" .

docker run -d --rm \
  --name "$container_name" \
  -p "${preview_port}:8080" \
  --tmpfs /app/config \
  --tmpfs /music \
  -e Subsonic__Url=http://127.0.0.1:1 \
  -e Subsonic__AutoDetectDownloadPath=false \
  -e Soulseek__BaseUrl=http://127.0.0.1:1 \
  -e Library__DownloadPath=/music \
  "$image_name" >/dev/null

for _ in {1..60}; do
  if curl -fsS "$preview_url" >/dev/null 2>&1; then
    echo "Preview ready: $preview_url"
    echo "Stop it with: ./scripts/preview-admin.sh stop"
    if [[ "${NO_OPEN:-0}" != 1 ]]; then
      if command -v open >/dev/null 2>&1; then open "$preview_url"
      elif command -v xdg-open >/dev/null 2>&1; then xdg-open "$preview_url"
      fi
    fi
    exit 0
  fi
  sleep 1
done

echo "Preview failed to start. Inspect it with: docker logs $container_name" >&2
exit 1
