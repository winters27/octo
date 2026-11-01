# Octo's Mornye: Mornye's own published image, pinned by digest (1.0.5, Mornye's release with its
# dependencies pinned), with Octo's launcher, assist helper and start wrapper beside it, so a
# person can pass Mornye's check from Octo's dashboard. Nothing of Mornye's is changed.
#
# MORNYE_IMAGE (docker-compose.yml passes SPOTIFLAC_IMAGE) runs another Mornye image under the
# same additions; unset or blank, it is the digest below, the only place it is written.
ARG MORNYE_IMAGE
FROM ${MORNYE_IMAGE:-ghcr.io/mornyeapp/mornye-python@sha256:8840ab3f1dd623ae8a5290057b35f9a512acc5339bf1e2c96a29ca3f9aaee65c}
# Marks every image built here, so an update can prune the old ones and nothing else.
LABEL app.octo.mornye=1
USER root
COPY octo-start octo-chromium assist.py /octo/
RUN chmod 0755 /octo/octo-start /octo/octo-chromium && chmod 0644 /octo/assist.py
USER mornye
# Mornye starts the browser for its check from CHROME_PATH.
ENV CHROME_PATH=/octo/octo-chromium
ENTRYPOINT ["/usr/bin/tini", "-g", "--", "/octo/octo-start"]
CMD ["--help"]
