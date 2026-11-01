# Vibeface www portal

`apps/www` builds the public portal for `www.vibeface.dev`. It contains the landing page, public downloads and community links, and the documentation pages sourced from the repository root `docs/` directory.

Install dependencies from the repository root, then run the portal locally:

```sh
pnpm install --frozen-lockfile
pnpm --filter vibeface-www dev
```

Run the checks and create the static output:

```sh
pnpm --filter vibeface-www run typecheck
pnpm --filter vibeface-www run build
pnpm --filter vibeface-www run check:static
pnpm --filter vibeface-www run test:e2e:check

# The e2e runner downloads Chromium on its first run if needed.
pnpm --filter vibeface-www run test:e2e
```

The deployable files are in `apps/www/dist/client`. Serve that directory with a static server to review deep links such as `/docs/install/`. The portal reuses the logo from `apps/app/public/icon.svg` in `public/logo.svg`, including the document navigation and favicon.

`.github/workflows/deploy-www.yml` dispatches `deploy-vibeface-www` to `leaperone/leaperone-releases` when matching changes reach `main`, or when the workflow runs manually. `apps/www/Dockerfile` builds a static Nginx container. `wrangler.toml` remains an optional Cloudflare Workers Static Assets configuration.

The portal does not include the user app, Node runtime, discovery service, credentials, or private workspace data. The public repository currently exposes Nightly Releases, so the download page links to the GitHub Releases page instead of claiming a stable package.
