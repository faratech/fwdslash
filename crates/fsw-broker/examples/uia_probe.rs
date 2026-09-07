//! Diagnostic probe: prints the UIA ancestry chain of the current focused
//! element. Focus the element to inspect (e.g. the Firefox urlbar), then run.
//! Temporary diagnostic tool for the Gecko urlbar admission investigation.

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationElement};
use windows::core::Result;

fn describe(depth: usize, element: &IUIAutomationElement) -> Result<()> {
    unsafe {
        let class = element.CurrentClassName()?.to_string();
        let automation_id = element.CurrentAutomationId()?.to_string();
        let name = element
            .CurrentName()
            .map(|n| n.to_string())
            .unwrap_or_default();
        let control_type = element.CurrentControlType()?.0;
        let native = element.CurrentNativeWindowHandle()?;
        let pid = element.CurrentProcessId()?;
        let padding = "  ".repeat(depth);
        println!(
            "{padding}[{depth}] class='{class}' automation_id='{automation_id}' name='{name}' \
             control_type={control_type} pid={pid} hwnd={:x}",
            native.0 as usize
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let focused: IUIAutomationElement = automation.GetFocusedElement()?;
        println!("focused element ancestry (focus something in the target app first):");
        let mut current = Some(focused);
        for depth in 0..10 {
            let Some(element) = current else { break };
            if describe(depth, &element).is_err() {
                break;
            }
            let walker = automation.ControlViewWalker()?;
            current = walker.GetParentElement(&element).ok();
        }
    }
    Ok(())
}
