# The MQTT broker of `make e2e`'s mqtt suite: Mosquitto with one user and no anonymous access, as a Home Assistant's
# broker has. Pinned by digest, the tag kept to read it, and Dependabot moves it.
FROM eclipse-mosquitto:2.1.2@sha256:860aeb5d63ce509508f6dcfb2bc8a1c1dd2bc60555b562b45ce90e52d8dabb0f

RUN printf 'listener 1883\nallow_anonymous false\npassword_file /mosquitto/config/passwd\n' > /mosquitto/config/mosquitto.conf \
    && mosquitto_passwd -b -c /mosquitto/config/passwd limen e2e-secret \
    && chown mosquitto:mosquitto /mosquitto/config/passwd \
    && chmod 0600 /mosquitto/config/passwd
