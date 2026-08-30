# Сборка образа veldb.
#
#   docker build -t veldb .
#   docker run --rm -p 8080:8080 -p 8081:8081 -v veldb-data:/var/lib/veldb veldb
#
# Мультиарх (amd64 + arm64) и публикация в GHCR — см. .github/workflows/release.yml.

FROM rust:1-bookworm AS builder

# protoc нужен на этапе сборки: build.rs генерирует gRPC-код из proto/veldb.proto.
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Зависимости отдельным слоем: они меняются куда реже своего кода, и без этого
# каждая правка в src/ пересобирала бы весь tonic заново.
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
RUN mkdir -p src/bin \
    && echo 'fn main() {}' > src/bin/server.rs \
    && cp src/bin/server.rs src/bin/cli.rs \
    && cp src/bin/server.rs src/bin/bench.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY bench ./bench
# Кэш зависимостей выше оставил в target/ артефакты пустышек — их надо выбросить,
# иначе cargo решит, что пересобирать нечего.
RUN touch src/lib.rs src/bin/*.rs \
    && cargo build --release --locked \
    && strip target/release/veldb target/release/veldbctl target/release/veldb-bench

FROM debian:bookworm-slim

# Ни curl, ни shell-утилит: HEALTHCHECK ходит через собственный клиент,
# и лишней поверхности атаки в образе не появляется.
RUN useradd --system --create-home --home-dir /var/lib/veldb --shell /usr/sbin/nologin veldb

COPY --from=builder /src/target/release/veldb /usr/local/bin/
COPY --from=builder /src/target/release/veldbctl /usr/local/bin/
COPY --from=builder /src/target/release/veldb-bench /usr/local/bin/
COPY --from=builder /src/bench /opt/veldb/bench

USER veldb
WORKDIR /var/lib/veldb
VOLUME ["/var/lib/veldb"]
EXPOSE 8080 8081

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["veldbctl", "-e", "SELECT 1"]

# 0.0.0.0, а не 127.0.0.1 по умолчанию: внутри контейнера localhost снаружи недостижим.
# --max-memory задайте под лимит контейнера: без него переполнение убивает процесс
# OOM-killer'ом вместо внятной ошибки на вставке.
ENTRYPOINT ["veldb"]
CMD ["--data", "/var/lib/veldb", "--http", "0.0.0.0:8080", "--grpc", "0.0.0.0:8081"]
