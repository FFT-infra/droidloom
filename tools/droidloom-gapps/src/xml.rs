//! Select package-scoped configuration; never import phone/Pixel feature claims.
use crate::util::*;
use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesStart, Event},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
struct Node {
    name: String,
    attributes: BTreeMap<String, String>,
    children: Vec<Node>,
}

impl Node {
    fn start(start: &BytesStart<'_>, reader: &Reader<&[u8]>) -> Result<Self> {
        let mut attributes = BTreeMap::new();
        for attr in start.attributes() {
            let attr = attr?;
            let key = String::from_utf8(attr.key.as_ref().to_vec())?;
            let value = attr
                .decode_and_unescape_value(reader.decoder())?
                .into_owned();
            if attributes.insert(key, value).is_some() {
                return Err("duplicate XML attribute".into());
            }
        }
        Ok(Self {
            name: String::from_utf8(start.name().as_ref().to_vec())?,
            attributes,
            children: vec![],
        })
    }
    fn write(&self, writer: &mut Writer<Vec<u8>>) -> Result<()> {
        let mut start = BytesStart::new(self.name.as_str());
        for (name, value) in &self.attributes {
            start.push_attribute((name.as_str(), value.as_str()));
        }
        if self.children.is_empty() {
            writer.write_event(Event::Empty(start))?;
        } else {
            writer.write_event(Event::Start(start))?;
            for child in &self.children {
                child.write(writer)?;
            }
            writer.write_event(Event::End(BytesEnd::new(self.name.as_str())))?;
        }
        Ok(())
    }
}

fn parse(bytes: &[u8]) -> Result<Node> {
    let mut reader = Reader::from_reader(bytes);
    let mut stack: Vec<Node> = vec![];
    let mut root = None;
    loop {
        let node = match reader.read_event()? {
            Event::Start(start) => {
                if stack.len() >= 16 {
                    return Err("XML nesting exceeds integration limit".into());
                }
                stack.push(Node::start(&start, &reader)?);
                continue;
            }
            Event::Empty(start) => Node::start(&start, &reader)?,
            Event::End(_) => stack.pop().ok_or("unmatched XML end tag")?,
            // Android's SystemConfig consumes elements/attributes and skips
            // character data between them. Some LiteGapps XML has a stray dot
            // between entries; it must not become a configuration directive.
            Event::Text(text)
                if !stack.is_empty() || text.as_ref().iter().all(u8::is_ascii_whitespace) =>
            {
                continue;
            }
            Event::Comment(_) | Event::Decl(_) => continue,
            Event::Eof => break,
            event => {
                return Err(format!(
                    "unsupported text, entity or DTD in Android configuration: {event:?}"
                )
                .into());
            }
        };
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node);
        } else if root.replace(node).is_some() {
            return Err("multiple XML roots".into());
        }
    }
    if !stack.is_empty() {
        return Err("unclosed XML element".into());
    }
    root.ok_or_else(|| "empty XML document".into())
}

/// Keep only selected packages' requested permissions and service configuration.
pub fn select(
    bytes: &[u8],
    partition: &str,
    packages: &BTreeMap<String, (String, BTreeSet<String>)>,
) -> Result<Option<Vec<u8>>> {
    let mut root = parse(bytes)?;
    if !["permissions", "config", "exceptions"].contains(&root.name.as_str()) {
        return Ok(None);
    }
    root.children.retain_mut(|node| {
        let Some(package) = node.attributes.get("package") else {
            return false;
        };
        let Some((owner_partition, requested)) = packages.get(package) else {
            return false;
        };
        match node.name.as_str() {
            "privapp-permissions" => {
                if owner_partition != partition {
                    return false;
                }
                node.children.retain(|child| {
                    ["permission", "deny-permission"].contains(&child.name.as_str())
                        && child
                            .attributes
                            .get("name")
                            .is_some_and(|name| requested.contains(name))
                });
                !node.children.is_empty()
            }
            "exception" => {
                node.children.retain(|child| {
                    child.name == "permission"
                        && child
                            .attributes
                            .get("name")
                            .is_some_and(|name| requested.contains(name))
                });
                !node.children.is_empty()
            }
            "allow-in-power-save"
            | "allow-in-power-save-except-idle"
            | "allow-in-data-usage-save"
            | "allow-unthrottled-location"
            | "allow-ignore-location-settings"
            | "app-link"
            | "hidden-api-whitelisted-app"
            | "whitelisted-staged-installer"
            | "install-in-user-type"
            | "component-override"
            | "allow-association" => true,
            _ => false,
        }
    });
    if root.children.is_empty() {
        return Ok(None);
    }
    // Retain source copyright/license comments even when their surrounding
    // configuration entries are excluded from the integration policy.
    let source = std::str::from_utf8(bytes)?;
    let mut notices = Vec::new();
    for comment in source.split("<!--").skip(1) {
        let text = comment.split_once("-->").ok_or("unclosed XML comment")?.0;
        notices.extend_from_slice(format!("<!--{text}-->\n").as_bytes());
    }
    let mut writer = Writer::new_with_indent(notices, b' ', 2);
    root.write(&mut writer)?;
    Ok(Some(writer.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packages() -> BTreeMap<String, (String, BTreeSet<String>)> {
        [(
            "com.google.android.gms".into(),
            (
                "product".into(),
                ["android.permission.INTERNET".into()].into(),
            ),
        )]
        .into()
    }
    #[test]
    fn excludes_pixel_claims_unselected_apps_and_unrequested_permissions() {
        let xml = br#"<permissions><feature name="com.google.android.feature.PIXEL_2025_EXPERIENCE"/><privapp-permissions package="com.google.android.gms"><permission name="android.permission.INTERNET"/><permission name="android.permission.MASTER_CLEAR"/></privapp-permissions><privapp-permissions package="com.google.android.setupwizard"><permission name="android.permission.INTERNET"/></privapp-permissions></permissions>"#;
        let filtered =
            String::from_utf8(select(xml, "product", &packages()).unwrap().unwrap()).unwrap();
        assert!(filtered.contains("INTERNET"));
        assert!(
            !filtered.contains("MASTER_CLEAR")
                && !filtered.contains("PIXEL")
                && !filtered.contains("setupwizard")
        );
        assert!(select(xml, "system_ext", &packages()).unwrap().is_none());
    }
    #[test]
    fn rejects_doctype_and_malformed_xml() {
        for xml in [
            "<!DOCTYPE p [<!ENTITY e SYSTEM 'file:///etc/passwd'>]><permissions/>",
            "<permissions><a></permissions>",
            "<permissions>",
        ] {
            assert!(select(xml.as_bytes(), "product", &packages()).is_err());
        }
    }

    #[test]
    fn ignores_in_element_character_data_like_android_system_config() {
        let input = br#"<permissions>.<privapp-permissions package="com.google.android.gms"><permission name="android.permission.INTERNET"/></privapp-permissions></permissions>"#;
        let selected = select(input, "product", &packages()).unwrap().unwrap();
        assert!(String::from_utf8(selected).unwrap().contains("INTERNET"));
        assert!(select(b"outside<permissions/>", "product", &packages()).is_err());
    }
}
