//! Deposit and fee payers, and the authorization returned by the LCD.

use cosmrs::AccountId;
use serde::Deserialize;

pub(super) enum ProposalFunding {
    Direct {
        signer: AccountId,
    },
    Authorized {
        signer: AccountId,
        granter: AccountId,
    },
}

impl ProposalFunding {
    pub fn signer(&self) -> &AccountId {
        match self {
            Self::Direct { signer } | Self::Authorized { signer, .. } => signer,
        }
    }

    pub fn proposer(&self) -> &AccountId {
        match self {
            Self::Direct { signer } => signer,
            Self::Authorized { granter, .. } => granter,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct GrantsResponse {
    pub grants: Vec<Grant>,
}

#[derive(Deserialize)]
pub(super) struct Grant {
    pub authorization: Authorization,
    pub expiration: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "@type")]
pub(super) enum Authorization {
    #[serde(rename = "/cosmos.authz.v1beta1.GenericAuthorization")]
    Generic { msg: String },
    #[serde(other)]
    Unsupported,
}
