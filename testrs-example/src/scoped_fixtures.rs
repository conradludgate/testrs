//! Reusing fixture names across scopes must preserve closest-scope resolution.

use testrs::{fixture, test};

#[fixture]
fn value() -> usize {
    10
}

#[fixture]
fn description(value: &usize) -> String {
    format!("value-{value}")
}

#[test]
fn root(value: &usize, description: &String) {
    assert_eq!(*value, 10);
    assert_eq!(description, "value-10");
}

pub mod nested {
    use testrs::{fixture, test};

    #[fixture]
    fn value() -> usize {
        20
    }

    #[fixture]
    fn description(value: &usize) -> String {
        format!("value-{value}")
    }

    #[test]
    fn child(value: &usize, description: &String) {
        assert_eq!(*value, 20);
        assert_eq!(description, "value-20");
    }

    pub mod grandchild {
        #[testrs::test]
        fn inherited(value: &usize, description: &String) {
            assert_eq!(*value, 20);
            assert_eq!(description, "value-20");
        }
    }
}

pub mod sibling {
    use testrs::{fixture, test};

    #[fixture]
    fn value() -> usize {
        30
    }

    #[fixture]
    fn description(value: &usize) -> String {
        format!("value-{value}")
    }

    #[test]
    fn child(value: &usize, description: &String) {
        assert_eq!(*value, 30);
        assert_eq!(description, "value-30");
    }
}
