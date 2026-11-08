FROM nginx:1.29-alpine
RUN apk add --no-cache python3
RUN rm -rf /usr/share/nginx/html && mkdir /usr/share/nginx/html
ARG STATIC_ROOT=web
COPY component/${STATIC_ROOT}/ /usr/share/nginx/html/
COPY component/COMPONENT-INFO.json runtime.py /opt/vibeface/
COPY nginx.conf /etc/nginx/nginx.conf
RUN python3 /opt/vibeface/runtime.py info > /dev/null
USER 101:101
EXPOSE 8080
STOPSIGNAL SIGQUIT
HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
  CMD ["python3", "/opt/vibeface/runtime.py", "health"]
ENTRYPOINT ["nginx"]
CMD ["-g", "daemon off;"]
