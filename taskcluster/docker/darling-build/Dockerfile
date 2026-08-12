FROM $DOCKER_IMAGE_PARENT
MAINTAINER Jamie Nicol <jnicol@mozilla.com>

VOLUME /builds/worker/checkouts
VOLUME /builds/worker/workspace
VOLUME /builds/worker/tooltool-cache

# Full list of packages required to build darling is documented at
# https://docs.darlinghq.org/build-instructions.html.
# We only build the "system" component, however, which requires significantly
# fewer dependencies.
RUN apt-get update && \
    apt-get install \
      libbsd-dev \
      libcap2-bin
