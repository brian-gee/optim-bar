//! Native WinRT toast notifications. As the shell replacement we can't use
//! tray balloons (we ARE the tray), but toasts render through the OS
//! notification center, which still belongs to Windows. Needs a registered
//! AppUserModelID, which we create under HKCU on startup.

use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};

const AUMID: &str = "optim.bar";

/// Register the AUMID so toasts display with our name. Idempotent.
pub fn ensure_registered() {
    unsafe {
        let name: Vec<u16> = "optim bar\0".encode_utf16().collect();
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            windows::core::w!("Software\\Classes\\AppUserModelId\\optim.bar"),
            windows::core::w!("DisplayName"),
            REG_SZ.0,
            Some(name.as_ptr() as _),
            (name.len() * 2) as u32,
        );
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn show(title: &str, body: &str) {
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\">\
         <text>{}</text><text>{}</text>\
         </binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    );
    let _ = (|| -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)?;
        Ok(())
    })();
}
