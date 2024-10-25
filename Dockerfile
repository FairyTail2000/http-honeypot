FROM rust:alpine3.20 AS builder

ENV HOME=/home/root
WORKDIR $HOME/app

RUN apk add musl-dev

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/home/root/app/target \
    cargo build --release && cp /home/root/app/target/release/http-honeypot /home/root/app/http-honeypot

FROM scratch

USER 1000:1000

COPY --from=builder --chown=1000:1000 /home/root/app/http-honeypot /http-honeypot

ENV PORT=8080

EXPOSE 8080

ENTRYPOINT ["/http-honeypot"]
