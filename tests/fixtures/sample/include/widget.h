#pragma once

namespace ui {

/// A simple widget for testing.
class Widget {
public:
    Widget();
    ~Widget();

    /// Draw the widget on screen.
    void Draw();

    /// Resize the widget.
    void Resize(int w, int h);

    /// Overloaded SetText method.
    void SetText(const char* text);
    void SetText(const char* text, int maxlen);

    int GetWidth() const;
    int GetHeight() const;

private:
    int width_;
    int height_;
    const char* text_;
};

/// A free function in the ui namespace.
void InitializeUI();

} // namespace ui
