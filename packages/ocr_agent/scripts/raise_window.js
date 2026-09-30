// 把标题/类名包含 target 的窗口提到最前（activate）。
// 通过 KWin Scripting 接口运行：loadScript + start。
const target = "testing_08";
const clients = workspace.clientList();
let found = false;
for (const c of clients) {
    const caption = c.caption || "";
    const cls = c.resourceClass || c.resourceName || "";
    if (caption.includes(target) || cls.includes(target)) {
        c.activate();
        found = true;
        print("raised window: caption=" + caption + " class=" + cls);
    }
}
if (!found) {
    print("no window matching: " + target);
}
