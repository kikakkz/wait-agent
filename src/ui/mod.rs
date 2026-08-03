pub mod banner;

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn box_drawing_characters_are_treated_as_single_width() {
        assert_eq!(UnicodeWidthStr::width("────"), 4);
    }

    #[test]
    fn footer_separators_are_treated_as_single_width() {
        assert_eq!(UnicodeWidthStr::width("·…"), 2);
    }
}
