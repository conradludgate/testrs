//! Reusing fixture names across scopes must preserve closest-scope resolution.

use testrs::{fixture, test};

#[fixture]
fn value() -> usize {
    10
}

#[fixture]
fn description(value: &usize) -> u64 {
    u64::try_from(*value).unwrap() + 100
}

#[test]
fn root(value: &usize, description: &u64) {
    assert_eq!(*value, 10);
    assert_eq!(*description, 110);
}

pub mod nested {
    use testrs::{fixture, test};

    #[fixture]
    fn value() -> usize {
        20
    }

    #[fixture]
    fn description(value: &usize) -> u64 {
        u64::try_from(*value).unwrap() + 100
    }

    #[test]
    fn child(value: &usize, description: &u64) {
        assert_eq!(*value, 20);
        assert_eq!(*description, 120);
    }

    pub mod grandchild {
        #[testrs::test]
        fn inherited(value: &usize, description: &u64) {
            assert_eq!(*value, 20);
            assert_eq!(*description, 120);
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
    fn description(value: &usize) -> u64 {
        u64::try_from(*value).unwrap() + 100
    }

    #[test]
    fn child(value: &usize, description: &u64) {
        assert_eq!(*value, 30);
        assert_eq!(*description, 130);
    }
}
