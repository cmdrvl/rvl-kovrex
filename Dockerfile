# Build stage
FROM rust:1.93-alpine AS builder

RUN apk add --no-cache musl-dev git

WORKDIR /app
COPY . .

RUN cargo build --release

# Runtime stage
FROM alpine:3.19

RUN apk add --no-cache ca-certificates

COPY --from=builder /app/target/release/rvl-kovrex /usr/local/bin/

# Railway uses PORT env var
ENV RVL_PORT=8080
EXPOSE 8080

CMD ["rvl-kovrex"]
