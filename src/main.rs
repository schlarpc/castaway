//! castaway: A Rust application built with Nix flakes.
//!
//! This is a skeleton project demonstrating best practices for Rust + Nix integration.

/// Application entry point.
fn main() {
    let message = greet("World");
    println!("{message}");
}

/// Generate a greeting message for the given name.
///
/// # Arguments
///
/// * `name` - The name to greet
///
/// # Returns
///
/// A formatted greeting string
fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}

/// Add two numbers together.
///
/// This is an example function to demonstrate testing.
#[cfg(test)]
fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    // Tests may unwrap freely; the production `unwrap_used` lint doesn't apply here.
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn test_greet_world() {
        assert_eq!(greet("World"), "Hello, World!");
    }

    #[test]
    fn test_greet_custom_name() {
        assert_eq!(greet("Rust"), "Hello, Rust!");
    }

    #[test]
    fn test_greet_empty_name() {
        assert_eq!(greet(""), "Hello, !");
    }

    #[test]
    fn test_add_positive_numbers() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn test_add_negative_numbers() {
        assert_eq!(add(-2, -3), -5);
    }

    #[test]
    fn test_add_mixed_numbers() {
        assert_eq!(add(-2, 5), 3);
    }

    #[test]
    fn test_add_zero() {
        assert_eq!(add(0, 0), 0);
        assert_eq!(add(5, 0), 5);
        assert_eq!(add(0, 5), 5);
    }
}
