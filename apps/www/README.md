# Vibeface www portal

`apps/www` builds the public portal for `www.vibeface.dev`. It contains the landing page, public downloads and community links, and the documentation pages sourced from the repository root `docs/` directory.

Public document links are registered in `src/lib/public-docs.ts`. Document bodies, titles, and descriptions come from `docs/`; edit those source files so the website and repository keep the same instructions.

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

`.github/workflows/deploy-www.yml` dispatches `deploy-vibeface-www` to `leaperone/leaperone-releases` after a successful stable `www-v*` release, or when explicitly run on that stable tag. Nightly and RC tags publish reviewable static artifacts. `apps/www/Dockerfile` builds a static Nginx container. `wrangler.toml` remains an optional Cloudflare Workers Static Assets configuration.

The portal does not include the user app, Node runtime, discovery service, credentials, or private workspace data. The public repository currently exposes Nightly Releases, so the download page links to the GitHub Releases page instead of claiming a stable package.
