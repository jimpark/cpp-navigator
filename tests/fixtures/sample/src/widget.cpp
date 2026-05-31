#include "widget.h"
#include <cstring>

namespace ui {

Widget::Widget() : width_(100), height_(50), text_(nullptr) {}

Widget::~Widget() {}

void Widget::Draw() {
    // Draw implementation
    if (text_) {
        // render text
    }
}

void Widget::Resize(int w, int h) {
    width_ = w;
    height_ = h;
}

void Widget::SetText(const char* text) {
    text_ = text;
}

void Widget::SetText(const char* text, int maxlen) {
    // Only set if within bounds
    if (maxlen > 0) {
        text_ = text;
    }
}

int Widget::GetWidth() const {
    return width_;
}

int Widget::GetHeight() const {
    return height_;
}

void InitializeUI() {
    // Global UI init
}

} // namespace ui
