FROM node@sha256:5647be709086c696ff32edaaf1c70cd26d1da6ab2b39c32f3c7b4c4a31957e37

RUN npm install --global --no-audit --no-fund @openai/codex@0.163.0 @anthropic-ai/claude-code@2.1.296 \
    && npm cache clean --force \
    && test "$(codex --version)" = 'codex-cli 0.163.0' \
    && test "$(claude --version)" = '2.1.296 (Claude Code)' \
    && /usr/bin/python3 -I -S -c 'import ctypes; assert ctypes.CDLL(None).prctl(4, 0, 0, 0, 0) == 0' \
    && useradd --uid 10001 --user-group --create-home --shell /bin/sh vibeface \
    && install -d -o vibeface -g vibeface /project

ENV HOME=/home/vibeface
ENV PATH=/usr/local/bin:/usr/bin:/bin
USER 10001:10001
WORKDIR /project
CMD ["codex", "--version"]
