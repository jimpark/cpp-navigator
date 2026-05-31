#include "widget.h"

void CreateAndUseWidgets() {
    ui::Widget w;
    w.Draw();
    w.SetText("hello");
    w.SetText("world", 5);
    w.Resize(200, 100);
}

void AnotherFunction() {
    ui::Widget panel;
    panel.Draw();
    panel.SetText("panel label");
    int width = panel.GetWidth();
    (void)width;
}

int main() {
    ui::InitializeUI();
    CreateAndUseWidgets();
    AnotherFunction();
    return 0;
}
