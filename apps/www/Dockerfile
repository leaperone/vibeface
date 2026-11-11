FROM node:24-bookworm-slim AS build

ENV COREPACK_HOME=/root/.cache/node/corepack
RUN corepack enable && corepack prepare pnpm@11.9.0 --activate
WORKDIR /workspace
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY apps/www/package.json apps/www/package.json
COPY packages packages
RUN pnpm install --frozen-lockfile
COPY apps/www apps/www
RUN pnpm --filter vibeface-www run typecheck \
 && pnpm --filter vibeface-www run build \
 && pnpm --filter vibeface-www run check:static

FROM nginx:1.29-alpine
COPY --from=build /workspace/apps/www/dist/client /usr/share/nginx/html
RUN printf '%s\n' \
  'server {' \
  '    listen 80;' \
  '    server_name _;' \
  '    root /usr/share/nginx/html;' \
  '    location / { try_files $uri $uri/ $uri/index.html =404; }' \
  '}' > /etc/nginx/conf.d/default.conf
