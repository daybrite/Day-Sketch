menu_file = File
menu_new = New Drawing
    .key = n
menu_open = Open…
    .key = o
menu_export = Export a Copy…
    .key = s
menu_edit = Edit
menu_undo = Undo
menu_redo = Redo
menu_arrange = Arrange
menu_group = Group
    .key = g
menu_ungroup = Ungroup
    .key = g
menu_forward = Bring Forward
    .key = ]
menu_backward = Send Backward
    .key = [
menu_front = Bring to Front
    .key = ]
menu_back = Send to Back
    .key = [
menu_delete = Delete

tool_shape = Shape
tool_rect = Rectangle
tool_oval = Oval
tool_line = Line
tool_text = Text
# A new text node's content.
text_default = Text
menu_insert = Insert
menu_cancel = Cancel

status_count = { $n } nodes
status_none = Nothing selected
status_selected = Selected: { $ids }
doc_untitled = Untitled

menu_view = View
menu_zoom_in = Zoom In
    .key = +
menu_zoom_out = Zoom Out
    .key = -
menu_zoom_reset = Actual Size
    .key = 0
status_zoom = { $pct }%
menu_inspector = Inspector
    .key = i
tool_inspector = Inspector
menu_layers = Layers
    .key = l
# The layers tree names its rows by kind: "Rectangle 3", where 3 is the node's id.
layer_rect = Rectangle { $n }
layer_oval = Oval { $n }
layer_line = Line { $n }
layer_text = Text { $n }
layer_group = Group { $n }
insp_tab_canvas = Canvas
# The Selected tab names its own contents: "No Items" / "1 Item" / "N Items". Fluent's plural
# selector picks the CLDR category for the locale, so a language with more than two forms
# (Polish's few/many, Arabic's six) adds them here without touching the app.
insp_tab_selected = { $n ->
    [0] No Items
    [one] { $n } Item
   *[other] { $n } Items
}
insp_background = Background
insp_geometry = Geometry
insp_x = X
insp_y = Y
insp_w = Width
insp_h = Height
insp_multi = multi
insp_done = Done
insp_style = Style
# The paint rows: a color well with its opacity beside it, so the labels stay one word.
insp_fill = Fill
insp_stroke = Stroke
insp_stroke_width = Thickness
insp_rotation = Rotation
insp_corner = Corners
# The Text section: a text node's content, family, style and size.
insp_text = Text
insp_content = Content
insp_font = Font
insp_size = Size
# The font menu's first entry: the platform's own face.
font_system = System
# The style menu, from a face's weight and slant.
style_regular = Regular
style_bold = Bold
style_italic = Italic
style_bold_italic = Bold Italic
style_ultralight = Ultra Light
style_thin = Thin
style_light = Light
style_medium = Medium
style_semibold = Semibold
style_heavy = Heavy
style_black = Black
style_weight_italic = { $weight } Italic
insp_degrees = { $deg }°
insp_percent = { $pct }%

undo_add_rect = Add Rectangle
undo_add_oval = Add Oval
undo_add_line = Add Line
undo_add_text = Add Text
# Preferences: the Editing section.
pref_editing = Editing
pref_snap = Snap to alignment guides
undo_edit_text = Edit Text
undo_move = Move
undo_resize = Resize
undo_group = Group
undo_ungroup = Ungroup
undo_arrange = Arrange
undo_reparent = Move Layer
undo_duplicate = Duplicate

# The selection's context menu (canvas right-click; layers-tree rows).
ctx_remove_group = Remove from Group
ctx_move_up = Move Up
ctx_move_down = Move Down
ctx_move_front = Move to Front
ctx_move_back = Move to Back
ctx_cut = Cut
ctx_copy = Copy
ctx_duplicate = Duplicate
ctx_paste = Paste
undo_delete = Delete
undo_cut = Cut
undo_paste = Paste
undo_style = Style
undo_background = Background
undo_rotate = Rotate
undo_corner = Corner Radius
