# The GloorpFlorp lobby server (see the README's "Lobby (optional)" section).
# CI publishes it as ghcr.io/munhyeonsua/gloorp-lobby for each release tag,
# which docker-compose.yml next to this file uses. By hand, from the
# repository root:
#   docker build -f packaging/lobby/Dockerfile -t gloorp-lobby .

FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY . .
# Only the lobby server, none of the app's Windows, audio or video parts.
RUN cargo build --release --locked -p gf-lobby --bin gloorp-lobby

FROM debian:trixie-slim
# Links the published image to the repository on GitHub.
LABEL org.opencontainers.image.source=https://github.com/munhyeonsua/GloorpFlorp
COPY --from=build /src/target/release/gloorp-lobby /usr/local/bin/gloorp-lobby
# Its own user; /data belongs to it, so a new volume mounted there does too.
RUN useradd --system --uid 10001 --no-create-home lobby && mkdir /data && chown lobby /data
USER lobby
ENV LOBBY_DATA=/data
VOLUME /data
EXPOSE 47900
CMD ["gloorp-lobby"]
