# Build from the repository root:
#   docker build -f crates/signal/Dockerfile -t lowlat-signal .
FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p rd-signal

FROM debian:bookworm-slim
COPY --from=build /src/target/release/rd-signal /usr/local/bin/rd-signal
EXPOSE 47010/tcp
EXPOSE 47010/udp
USER nobody
ENTRYPOINT ["rd-signal"]
CMD ["--listen", "0.0.0.0:47010"]
