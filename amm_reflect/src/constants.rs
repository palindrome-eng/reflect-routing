use solana_sdk::{pubkey, pubkey::Pubkey};

pub const REFLECT_LABEL: &str = "ReflectAmm";

pub mod reflect {
    use super::*;
    pub const ID: Pubkey = pubkey!("rf1ctRXK4bmG5XNttAMYfB3TKd2vQjFv5cfQhDBxdAQ");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod drift {
    use super::*;
    pub const ID: Pubkey = pubkey!("dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod reflect_main {
    use super::*;
    pub const ID: Pubkey = pubkey!("4BXzppSAgWDHmcN7AwMAmDphJj3BFdbCFo3Sos2Vms6v");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod admin_permissions {
    use super::*;
    pub const ID: Pubkey = pubkey!("F4NHcFQxvc6Kw1m6c2UmvmcwExcQkMcXDd7d4yXDow7S");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod controller_usdc_ata {
    use super::*;
    pub const ID: Pubkey = pubkey!("DRZE8YVE3UfsYzfTaPFKaNPtwG5BpYRBRXi6rEtzm3s5");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod controller_usdt_ata {
    use super::*;
    pub const ID: Pubkey = pubkey!("Gr6hPgmHQjWbWp1rHv5DddhQE3zpaRTfxu6Nz35PzHgP");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod referrer_user {
    use super::*;
    pub const ID: Pubkey = pubkey!("9XuTCfYKnecKCw4sSYZXdnR85miLuVWETZSMBRMTNVrj");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod reflect_user_account_strategy_0 {
    use super::*;
    pub const ID: Pubkey = pubkey!("F82oESzqX9fSGf9Sf3SP8PUfCGM9qUvHBAtvhZTqZrvt");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod drift_spot_market_vault {
    use super::*;
    pub const ID: Pubkey = pubkey!("GXWqPpjQpdz7KZw9p7f5PX2eGxHAhvpNXiviFkAB8zXg");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod usdc_mint {
    use super::*;
    pub const ID: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod usdt_mint {
    use super::*;
    pub const ID: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
    pub fn id() -> Pubkey {
        ID
    }
}

pub mod token_program {
    use super::*;
    pub const ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    pub fn id() -> Pubkey {
        ID
    }
}
