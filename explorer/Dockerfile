# DAVINCI explorer: the Vite build served by nginx. The entrypoint renders
# /config.json and the nginx site from environment variables at start, so one
# image serves any deployment (see README.md, "Docker").
#
#   docker build -t davinci-explorer explorer/
#   docker run -p 8080:8080 davinci-explorer

FROM node:22-bookworm-slim AS build
WORKDIR /app
RUN corepack enable && corepack prepare pnpm@10.0.0 --activate
COPY package.json pnpm-lock.yaml ./
RUN --mount=type=cache,target=/root/.local/share/pnpm/store pnpm install --frozen-lockfile
COPY . .
ARG VITE_BUILD_VERSION=dev
ENV VITE_BUILD_VERSION=${VITE_BUILD_VERSION}
RUN pnpm build

FROM nginx:1.27-alpine
RUN apk add --no-cache jq
COPY --from=build /app/dist /usr/share/nginx/html
# The committed public/config.json: the defaults every unset variable keeps.
COPY --from=build /app/public/config.json /etc/davinci-explorer/config.defaults.json
COPY --chmod=0755 docker/render.sh /docker-entrypoint.d/40-davinci-explorer.sh
ENV PORT=8080
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD wget -q -O /dev/null "http://127.0.0.1:${PORT}/healthz" || exit 1
