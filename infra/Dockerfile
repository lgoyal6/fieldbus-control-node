FROM rust:1.90-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p fieldbus-cloud -j 2

FROM debian:bookworm-slim
COPY --from=build /src/target/release/fieldbus-cloud /usr/local/bin/fieldbus-cloud
RUN useradd --system --home-dir /var/lib/fieldbus fieldbus && mkdir -p /var/lib/fieldbus && chown fieldbus:fieldbus /var/lib/fieldbus
USER fieldbus
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/fieldbus-cloud", "serve", "0.0.0.0:8080", "/var/lib/fieldbus/events.log"]
