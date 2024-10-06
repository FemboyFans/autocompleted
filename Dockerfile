FROM rust:1.75-alpine3.19 AS builder

WORKDIR /app

RUN apk add --no-cache musl-dev

COPY . .

RUN cargo build --release

FROM alpine:3.19

HEALTHCHECK CMD sh -c 'wget --no-verbose --tries=1 --spider http://$SERVER_ADDR/up || exit 1'

COPY --from=builder /app/target/release/autocompleted /app/autocompleted
