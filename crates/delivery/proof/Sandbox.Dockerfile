FROM risczero/risc0-groth16-prover@sha256:a4f80ce2e0b8e2bb7637a93c37136a6776ac00ec843a3fdf1c67b1d5ffea64ee AS groth16
FROM node:24.16.0-bookworm-slim@sha256:2c87ef9bd3c6a3bd4b472b4bec2ce9d16354b0c574f736c476489d09f560a203 AS node
FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e

ARG TARGETARCH
RUN test "$TARGETARCH" = amd64 \
    && rustup component add rustfmt --toolchain 1.98.1

COPY --from=groth16 /app /opt/vdp/gnark
COPY --from=groth16 /usr/local/bin/prover /usr/local/bin/stark_verify /opt/vdp/bin/
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm

RUN set -eu; \
    ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm; \
    ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx; \
    ln -s /opt/vdp/gnark/stark_verify.dat /opt/vdp/bin/stark_verify.dat; \
    curl --fail --location --retry 3 --proto '=https' --tlsv1.2 \
        https://github.com/risc0/risc0/releases/download/v3.0.6/cargo-risczero-x86_64-unknown-linux-gnu.tgz \
        --output /tmp/vdp-r0vm.tgz; \
    echo '615d961bfb81d318db5071d7548389c850e324ac7f421c075176daf26082a60a  /tmp/vdp-r0vm.tgz' | sha256sum --check --status; \
    tar -xzf /tmp/vdp-r0vm.tgz -C /opt/vdp/bin r0vm; \
    rm /tmp/vdp-r0vm.tgz; \
    test "$(/opt/vdp/bin/r0vm --version)" = 'risc0-r0vm 3.0.6'; \
    test "$(node --version)" = 'v24.16.0'; \
    python3 --version; \
    npm --version

COPY --chmod=0555 crates/delivery/proof/confined-gnark.sh /usr/bin/docker

ENV CARGO_HOME=/cargo \
    RISC0_HOME=/risc0 \
    RISC0_SERVER_PATH=/opt/vdp/bin/r0vm \
    PATH=/usr/local/cargo/bin:/opt/vdp/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

WORKDIR /work
ENTRYPOINT []
CMD ["bash"]
