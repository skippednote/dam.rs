# dam.rs web (SvelteKit + adapter-node). Built from the REPO ROOT as context, because the /tour route
# imports a brand asset from ../../../../docs (outside web/). Layout mirrors the repo: /repo/web + /repo/docs.
#   docker buildx build -f web/Dockerfile --platform linux/arm64 -t <img> --load .
FROM node:24-slim AS build
WORKDIR /repo/web
ENV PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
RUN npm install -g pnpm@10
COPY web/package.json web/pnpm-lock.yaml web/.npmrc ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
COPY docs/ /repo/docs/
RUN pnpm build && pnpm prune --prod

FROM node:24-slim
WORKDIR /app
ENV NODE_ENV=production PORT=3000 HOST=0.0.0.0
COPY --from=build /repo/web/build ./build
COPY --from=build /repo/web/node_modules ./node_modules
COPY --from=build /repo/web/package.json ./
EXPOSE 3000
CMD ["node", "build"]
