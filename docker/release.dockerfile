FROM scratch

COPY --from=binary --chmod=0755 tact /tact
