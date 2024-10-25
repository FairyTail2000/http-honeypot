FROM rust:alpine3.20 AS builder

WORKDIR /app

RUN apk add musl-dev

COPY . .

RUN cargo build --release

FROM scratch

USER 1000:1000

COPY --from=builder --chown=1000:1000 /app/target/release/http-honeypot /http-honeypot

ENV PORT=8080

EXPOSE 8080

ENTRYPOINT ["/http-honeypot"]
