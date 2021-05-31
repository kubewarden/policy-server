# build image
FROM rust:1.50-buster as builder

WORKDIR /usr/src/policy-server
COPY . .
RUN cargo install --path .

# final image
FROM gcr.io/distroless/cc:nonroot
LABEL org.opencontainers.image.source https://github.com/kubewarden/policy-server

COPY --from=builder /usr/local/cargo/bin/policy-server /

EXPOSE 3000

CMD ["./policy-server"]
