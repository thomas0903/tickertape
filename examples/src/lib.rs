//! End-to-end examples for tickertape. Content arrives in later milestones (see `docs/roadmap.md`).

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_tickertape() {
        assert!(!tickertape::VERSION.is_empty());
    }
}
