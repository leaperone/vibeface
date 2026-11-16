# Vibeface www portal

`apps/www` builds the public portal for `www.vibeface.dev`. It contains the landing page, public downloads and community links, and the documentation pages sourced from the repository root `packages/docs/` directory.

`public-docs.json` lists public document slugs in navigation order. Each slug names a `packages/docs/<slug>.md` source file and a `/docs/<slug>` route. Files absent from this registry stay out of the portal and its search index. Document bodies, titles, and descriptions come from `packages/docs/`.

After changing the registry, run `pnpm --filter vibeface-www generate:docs`. This generates `packages/docs/meta.json`, `src/lib/public-docs.ts`, and the literal file list required by the Fumadocs macro in `src/lib/source.ts`. Do not edit those generated files. `pnpm --filter vibeface-www check:docs` checks for stale output without writing files; typecheck and build also run this check, including in CI.

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

Publishing a complete `www-v<version>` release automatically dispatches the website deployment through this repository’s `deploy-services.yml`. For a retry or explicit rollout, `python3 scripts/deploy.py release --service www --tag www-v<version>` dispatches the same fixed entry. The deployment packages verified website bytes in a static container; no other repository is involved. `apps/www/Dockerfile` remains the source-build container for local development.

The portal does not include the user app, Node runtime, public node, credentials, or private workspace data. The public repository currently exposes hourly product Releases, so the download page links to the GitHub Releases page instead of claiming a stable package.
