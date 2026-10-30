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

# Chromium must already be installed for the e2e command.
pnpm --filter vibeface-www run test:e2e
```

The deployable files are in `apps/www/dist/client`. Serve that directory with a static server to review deep links such as `/docs/install/`. `wrangler.toml` describes a Cloudflare Workers Static Assets deployment, but this repository does not deploy the site automatically.

The portal does not include the user app, Node runtime, discovery service, credentials, or private workspace data. The public repository currently exposes Nightly Releases, so the download page links to the GitHub Releases page instead of claiming a stable package.
