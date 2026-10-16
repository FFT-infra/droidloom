//! Perfetto uses local Unix sockets in a shared-kernel Android cell.
//!
//! Some base images also configure an incoming VM relay endpoint. `traced`
//! treats failure to create that optional AF_VSOCK listener as fatal, even
//! though its Android producer/consumer sockets work. Override that image
//! property in the last-loaded product properties; keep tracing enabled.

pub(crate) fn container_properties(original: &str) -> String {
    let mut output = String::with_capacity(original.len() + 160);
    for line in original.lines() {
        if line
            .split_once('=')
            .is_some_and(|(key, _)| key.trim() == "traced.relay_producer_port")
        {
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }
    output.push_str("traced.relay_producer_port=\n");
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disables_only_the_vm_listener_and_preserves_local_tracing() {
        let input = "# image defaults\npersist.traced.enable=1\ntraced.relay_producer_port=vsock://-1:2010\nro.product.name=droidloom\ntraced.relay_producer_port=vsock://-1:2011\n";
        let result = container_properties(input);
        assert_eq!(
            result,
            "# image defaults\npersist.traced.enable=1\nro.product.name=droidloom\ntraced.relay_producer_port=\n"
        );
        assert_eq!(container_properties(&result), result);
    }

    #[test]
    fn overrides_an_inherited_vendor_default_without_requiring_a_product_entry() {
        let result = container_properties("ro.product.name=droidloom");
        assert_eq!(
            result,
            "ro.product.name=droidloom\ntraced.relay_producer_port=\n"
        );
    }
}
