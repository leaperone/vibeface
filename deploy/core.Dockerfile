FROM ubuntu:24.04
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates libgcc-s1 python3 \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /opt/vibeface
COPY component/ ./
COPY runtime.py ./runtime.py
RUN python3 runtime.py info > /dev/null
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=15s --start-period=20s --retries=3 \
  CMD ["python3", "/opt/vibeface/runtime.py", "health"]
ENTRYPOINT ["python3", "/opt/vibeface/runtime.py"]
CMD ["start"]
