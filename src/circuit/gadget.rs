use pasta_curves::pallas;

use ecc::chip::EccChip;
use poseidon::Pow5T3Chip as PoseidonChip;
use sinsemilla::{chip::SinsemillaChip, merkle::chip::MerkleChip};

pub mod ecc;
pub(crate) mod poseidon;
pub(crate) mod sinsemilla;
pub mod utilities;

impl super::Config {
    /// documentation
    pub(super) fn ecc_chip(&self) -> EccChip {
        EccChip::construct(self.ecc_config.clone())
    }

    /// documentation
    pub(super) fn sinsemilla_chip_1(&self) -> SinsemillaChip {
        SinsemillaChip::construct(self.sinsemilla_config_1.clone())
    }

    /// documentation
    pub(super) fn sinsemilla_chip_2(&self) -> SinsemillaChip {
        SinsemillaChip::construct(self.sinsemilla_config_2.clone())
    }

    /// documentation
    pub(super) fn merkle_chip_1(&self) -> MerkleChip {
        MerkleChip::construct(self.merkle_config_1.clone())
    }

    /// documentation
    pub(super) fn merkle_chip_2(&self) -> MerkleChip {
        MerkleChip::construct(self.merkle_config_2.clone())
    }

    /// documentation
    pub(super) fn poseidon_chip(&self) -> PoseidonChip<pallas::Base> {
        PoseidonChip::construct(self.poseidon_config.clone())
    }
}
