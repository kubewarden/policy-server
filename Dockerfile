# This is a Dockerfile that can be used to build the policy-server
# image on a local developer workstation.
# It could be used to build the official containers inside of GitHub actions,
# but this would be impractical because the ARM64 build would take ~ 3 hours
# and half (no kidding!).

FROM rust:1.70 AS build
WORKDIR /usr/src

# Download the target for static linking.
RUN rustup target add $(arch)-unknown-linux-musl

# Fix ring building using musl - see https://github.com/briansmith/ring/issues/1414#issuecomment-1055177218
RUN apt-get update && apt-get install musl-tools clang llvm -y
ENV CC="clang"

RUN mkdir /usr/src/policy-server
WORKDIR /usr/src/policy-server

# Building policy-server takes ~8 minutes on a fast machine,
# we don't want to rebuild that unless something changed inside of its codebase.
# Because of that we have to make a smart use of Docker cache system.

# Copy files shared by policy-server and policy-optimizer
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p crates

# Build policy-server first
COPY crates/policy-server crates/policy-server
# Create an empty rust project for policy-optimizer. This is required because the
# top-level Cargo.toml references the project as part of the workspace. There must
# be some valid Cargo.toml in there, otherwise building policy-server will fail
RUN cd crates/ && cargo new --bin policy-optimizer
RUN cargo build --release --target $(arch)-unknown-linux-musl --bin policy-server
RUN cp target/$(arch)-unknown-linux-musl/release/policy-server policy-server

# Build policy-optimizer, start by removing the fake project that was created above
RUN rm -rf crates/policy-optimizer
COPY crates/policy-optimizer crates/policy-optimizer
RUN cargo build --release --target $(arch)-unknown-linux-musl --bin policy-optimizer
RUN cp target/$(arch)-unknown-linux-musl/release/policy-optimizer policy-optimizer

FROM alpine AS cfg
RUN echo "policy-server:x:65533:65533::/tmp:/sbin/nologin" >> /etc/passwd
RUN echo "policy-server:x:65533:policy-server" >> /etc/group

# Copy the statically-linked binaries into a scratch container.
FROM scratch
COPY --from=cfg /etc/passwd /etc/passwd
COPY --from=cfg /etc/group /etc/group
COPY --from=build --chmod=0755 /usr/src/policy-server/policy-server /policy-server
COPY --from=build --chmod=0755 /usr/src/policy-server/policy-optimizer /policy-optimizer
# The Cargo.lock contains both the dependencies of policy-server and policy-optimizer
ADD Cargo.lock /Cargo.lock
USER 65533:65533
EXPOSE 3000
ENTRYPOINT ["/policy-server"]
