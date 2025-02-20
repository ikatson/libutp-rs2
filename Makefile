all:

bindgen:
	cd libutp-rs2-sys && \
	bindgen \
        --use-core \
        libutp/utp.h \
        --allowlist-item "^(utp|UTP|SHUT_).*" \
        --anon-fields-prefix "unnamed_field" \
        --opaque-type "socklen_t" \
        --blocklist-type "sockaddr" \
        --output src/bindings.rs
