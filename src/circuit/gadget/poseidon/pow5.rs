use std::convert::TryInto;
use std::iter;

use halo2::{
    arithmetic::FieldExt,
    circuit::{Cell, Chip, Layouter, Region},
    plonk::{Advice, Column, ConstraintSystem, Error, Expression, Fixed, Selector},
    poly::Rotation,
};

use super::{PoseidonDuplexInstructions, PoseidonInstructions};
use crate::circuit::gadget::utilities::{CellValue, Var};
use crate::primitives::poseidon::{Domain, Mds, Spec, SpongeState, State};

/// Configuration for a [`Pow5Chip`].
#[derive(Clone, Debug)]
pub struct Pow5Config<F: FieldExt, const WIDTH: usize, const RATE: usize> {
    pub(in crate::circuit) state: [Column<Advice>; WIDTH],
    partial_sbox: Column<Advice>,
    rc_a: [Column<Fixed>; WIDTH],
    rc_b: [Column<Fixed>; WIDTH],
    s_full: Selector,
    s_partial: Selector,
    s_pad_and_add: Selector,

    half_full_rounds: usize,
    half_partial_rounds: usize,
    alpha: [u64; 4],
    round_constants: Vec<[F; WIDTH]>,
    m_reg: Mds<F, WIDTH>,
    m_inv: Mds<F, WIDTH>,
}

/// A Poseidon chip using an $x^5$ S-Box, with a width of 3, suitable for a 2:1 reduction.
#[derive(Debug)]
pub struct Pow5Chip<F: FieldExt, const WIDTH: usize, const RATE: usize> {
    config: Pow5Config<F, WIDTH, RATE>,
}

impl<F: FieldExt, const WIDTH: usize, const RATE: usize> Pow5Chip<F, WIDTH, RATE> {
    /// Configures this chip for use in a circuit.
    ///
    /// # Side-effects
    ///
    /// All columns in `state` will be equality-enabled.
    //
    // TODO: Does the rate need to be hard-coded here, or only the width? It probably
    // needs to be known wherever we implement the hashing gadget, but it isn't strictly
    // necessary for the permutation.
    pub fn configure<S: Spec<F, WIDTH, RATE>>(
        meta: &mut ConstraintSystem<F>,
        state: [Column<Advice>; WIDTH],
        partial_sbox: Column<Advice>,
        rc_a: [Column<Fixed>; WIDTH],
        rc_b: [Column<Fixed>; WIDTH],
    ) -> Pow5Config<F, WIDTH, RATE> {
        assert_eq!(RATE, WIDTH - 1);
        // Generate constants for the Poseidon permutation.
        // This gadget requires R_F and R_P to be even.
        assert!(S::full_rounds() & 1 == 0);
        assert!(S::partial_rounds() & 1 == 0);
        let half_full_rounds = S::full_rounds() / 2;
        let half_partial_rounds = S::partial_rounds() / 2;
        let (round_constants, m_reg, m_inv) = S::constants();

        // This allows state words to be initialized (by constraining them equal to fixed
        // values), and used in a permutation from an arbitrary region. rc_a is used in
        // every permutation round, while rc_b is empty in the initial and final full
        // rounds, so we use rc_b as "scratch space" for fixed values (enabling potential
        // layouter optimisations).
        for column in iter::empty()
            .chain(state.iter().cloned().map(|c| c.into()))
            .chain(rc_b.iter().cloned().map(|c| c.into()))
        {
            meta.enable_equality(column);
        }

        let s_full = meta.selector();
        let s_partial = meta.selector();
        let s_pad_and_add = meta.selector();

        let alpha = [5, 0, 0, 0];
        let pow_5 = |v: Expression<F>| {
            let v2 = v.clone() * v.clone();
            v2.clone() * v2 * v
        };

        meta.create_gate("full round", |meta| {
            let s_full = meta.query_selector(s_full);

            (0..WIDTH)
                .map(|next_idx| {
                    let state_next = meta.query_advice(state[next_idx], Rotation::next());
                    let expr = (0..WIDTH).fold(-state_next, |acc, idx| {
                        let state_cur = meta.query_advice(state[idx], Rotation::cur());
                        let rc_a = meta.query_fixed(rc_a[idx], Rotation::cur());
                        acc + pow_5(state_cur + rc_a) * m_reg[next_idx][idx]
                    });
                    s_full.clone() * expr
                })
                .collect::<Vec<_>>()
        });

        meta.create_gate("partial rounds", |meta| {
            let cur_0 = meta.query_advice(state[0], Rotation::cur());
            let mid_0 = meta.query_advice(partial_sbox, Rotation::cur());

            let rc_a0 = meta.query_fixed(rc_a[0], Rotation::cur());
            let rc_b0 = meta.query_fixed(rc_b[0], Rotation::cur());

            let s_partial = meta.query_selector(s_partial);

            use halo2::plonk::VirtualCells;
            let mid = |idx: usize, meta: &mut VirtualCells<F>| {
                let mid = mid_0.clone() * m_reg[idx][0];
                (1..WIDTH).fold(mid, |acc, cur_idx| {
                    let cur = meta.query_advice(state[cur_idx], Rotation::cur());
                    let rc_a = meta.query_fixed(rc_a[cur_idx], Rotation::cur());
                    acc + (cur + rc_a) * m_reg[idx][cur_idx]
                })
            };

            let next = |idx: usize, meta: &mut VirtualCells<F>| {
                let next_0 = meta.query_advice(state[0], Rotation::next());
                let next_0 = next_0 * m_inv[idx][0];
                (1..WIDTH).fold(next_0, |acc, next_idx| {
                    let next = meta.query_advice(state[next_idx], Rotation::next());
                    acc + next * m_inv[idx][next_idx]
                })
            };

            let partial_round_linear = |idx: usize, meta: &mut VirtualCells<F>| {
                let expr = {
                    let rc_b = meta.query_fixed(rc_b[idx], Rotation::cur());
                    mid(idx, meta) + rc_b - next(idx, meta)
                };
                s_partial.clone() * expr
            };

            std::iter::empty()
                // state[0] round a
                .chain(Some(
                    s_partial.clone() * (pow_5(cur_0 + rc_a0) - mid_0.clone()),
                ))
                // state[0] round b
                .chain(Some(
                    s_partial.clone() * (pow_5(mid(0, meta) + rc_b0) - next(0, meta)),
                ))
                .chain((1..WIDTH).map(|idx| partial_round_linear(idx, meta)))
                .collect::<Vec<_>>()
        });

        meta.create_gate("pad-and-add", |meta| {
            let initial_state_rate = meta.query_advice(state[RATE], Rotation::prev());
            let output_state_rate = meta.query_advice(state[RATE], Rotation::next());

            let s_pad_and_add = meta.query_selector(s_pad_and_add);

            let pad_and_add = |idx: usize| {
                let initial_state = meta.query_advice(state[idx], Rotation::prev());
                let input = meta.query_advice(state[idx], Rotation::cur());
                let output_state = meta.query_advice(state[idx], Rotation::next());

                // We pad the input by storing the required padding in fixed columns and
                // then constraining the corresponding input columns to be equal to it.
                s_pad_and_add.clone() * (initial_state + input - output_state)
            };

            (0..RATE)
                .map(pad_and_add)
                // The capacity element is never altered by the input.
                .chain(Some(
                    s_pad_and_add.clone() * (initial_state_rate - output_state_rate),
                ))
                .collect::<Vec<_>>()
        });

        Pow5Config {
            state,
            partial_sbox,
            rc_a,
            rc_b,
            s_full,
            s_partial,
            s_pad_and_add,
            half_full_rounds,
            half_partial_rounds,
            alpha,
            round_constants,
            m_reg,
            m_inv,
        }
    }

    pub fn construct(config: Pow5Config<F, WIDTH, RATE>) -> Self {
        Pow5Chip { config }
    }
}

impl<F: FieldExt, const WIDTH: usize, const RATE: usize> Chip<F> for Pow5Chip<F, WIDTH, RATE> {
    type Config = Pow5Config<F, WIDTH, RATE>;
    type Loaded = ();

    fn config(&self) -> &Self::Config {
        &self.config
    }

    fn loaded(&self) -> &Self::Loaded {
        &()
    }
}

impl<F: FieldExt, S: Spec<F, WIDTH, RATE>, const WIDTH: usize, const RATE: usize>
    PoseidonInstructions<F, S, WIDTH, RATE> for Pow5Chip<F, WIDTH, RATE>
{
    type Word = StateWord<F>;

    fn permute(
        &self,
        layouter: &mut impl Layouter<F>,
        initial_state: &State<Self::Word, WIDTH>,
    ) -> Result<State<Self::Word, WIDTH>, Error> {
        let config = self.config();

        layouter.assign_region(
            || "permute state",
            |mut region| {
                // Load the initial state into this region.
                let state = Pow5State::load(&mut region, config, initial_state)?;

                let state = (0..config.half_full_rounds).fold(Ok(state), |res, r| {
                    res.and_then(|state| state.full_round(&mut region, config, r, r))
                })?;

                let state = (0..config.half_partial_rounds).fold(Ok(state), |res, r| {
                    res.and_then(|state| {
                        state.partial_round(
                            &mut region,
                            config,
                            config.half_full_rounds + 2 * r,
                            config.half_full_rounds + r,
                        )
                    })
                })?;

                let state = (0..config.half_full_rounds).fold(Ok(state), |res, r| {
                    res.and_then(|state| {
                        state.full_round(
                            &mut region,
                            config,
                            config.half_full_rounds + 2 * config.half_partial_rounds + r,
                            config.half_full_rounds + config.half_partial_rounds + r,
                        )
                    })
                })?;

                Ok(state.0)
            },
        )
    }
}

impl<F: FieldExt, S: Spec<F, WIDTH, RATE>, const WIDTH: usize, const RATE: usize>
    PoseidonDuplexInstructions<F, S, WIDTH, RATE> for Pow5Chip<F, WIDTH, RATE>
{
    fn initial_state(
        &self,
        layouter: &mut impl Layouter<F>,
        domain: &impl Domain<F, WIDTH, RATE>,
    ) -> Result<State<Self::Word, WIDTH>, Error> {
        let config = self.config();
        let state = layouter.assign_region(
            || format!("initial state for domain {:?}", domain),
            |mut region| {
                let mut state = Vec::with_capacity(WIDTH);
                let mut load_state_word = |i: usize, value: F| -> Result<_, Error> {
                    let var = region.assign_advice_from_constant(
                        || format!("state_{}", i),
                        config.state[i],
                        0,
                        value,
                    )?;
                    state.push(StateWord {
                        var,
                        value: Some(value),
                    });

                    Ok(())
                };

                for i in 0..RATE {
                    load_state_word(i, F::zero())?;
                }
                load_state_word(RATE, domain.initial_capacity_element())?;

                Ok(state)
            },
        )?;

        Ok(state.try_into().unwrap())
    }

    fn pad_and_add(
        &self,
        layouter: &mut impl Layouter<F>,
        domain: &impl Domain<F, WIDTH, RATE>,
        initial_state: &State<Self::Word, WIDTH>,
        input: &SpongeState<Self::Word, RATE>,
    ) -> Result<State<Self::Word, WIDTH>, Error> {
        let config = self.config();
        layouter.assign_region(
            || format!("pad-and-add for domain {:?}", domain),
            |mut region| {
                config.s_pad_and_add.enable(&mut region, 1)?;

                // Load the initial state into this region.
                let load_state_word = |i: usize| {
                    let value = initial_state[i].value;
                    let var = region.assign_advice(
                        || format!("load state_{}", i),
                        config.state[i],
                        0,
                        || value.ok_or(Error::SynthesisError),
                    )?;
                    region.constrain_equal(initial_state[i].var, var)?;
                    Ok(StateWord { var, value })
                };
                let initial_state: Result<Vec<_>, Error> =
                    (0..WIDTH).map(load_state_word).collect();
                let initial_state = initial_state?;

                let padding_values = domain.padding();

                // Load the input and padding into this region.
                let load_input_word = |i: usize| {
                    let (constraint_var, value) = match (input[i], padding_values[i]) {
                        (Some(word), None) => (word.var, word.value),
                        (None, Some(padding_value)) => {
                            let padding_var = region.assign_fixed(
                                || format!("load pad_{}", i),
                                config.rc_b[i],
                                1,
                                || Ok(padding_value),
                            )?;
                            (padding_var, Some(padding_value))
                        }
                        _ => panic!("Input and padding don't match"),
                    };
                    let var = region.assign_advice(
                        || format!("load input_{}", i),
                        config.state[i],
                        1,
                        || value.ok_or(Error::SynthesisError),
                    )?;
                    region.constrain_equal(constraint_var, var)?;

                    Ok(StateWord { var, value })
                };
                let input: Result<Vec<_>, Error> = (0..RATE).map(load_input_word).collect();
                let input = input?;

                // Constrain the output.
                let constrain_output_word = |i: usize| {
                    let value = initial_state[i].value.and_then(|initial_word| {
                        input
                            .get(i)
                            .map(|word| word.value)
                            // The capacity element is never altered by the input.
                            .unwrap_or_else(|| Some(F::zero()))
                            .map(|input_word| initial_word + input_word)
                    });
                    let var = region.assign_advice(
                        || format!("load output_{}", i),
                        config.state[i],
                        2,
                        || value.ok_or(Error::SynthesisError),
                    )?;
                    Ok(StateWord { var, value })
                };

                let output: Result<Vec<_>, Error> = (0..WIDTH).map(constrain_output_word).collect();
                output.map(|output| output.try_into().unwrap())
            },
        )
    }

    fn get_output(state: &State<Self::Word, WIDTH>) -> SpongeState<Self::Word, RATE> {
        state[..RATE]
            .iter()
            .map(|word| Some(*word))
            .collect::<Vec<_>>()
            .try_into()
            .unwrap()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StateWord<F: FieldExt> {
    var: Cell,
    value: Option<F>,
}

impl<F: FieldExt> StateWord<F> {
    pub fn new(var: Cell, value: Option<F>) -> Self {
        Self { var, value }
    }
}

impl<F: FieldExt> From<StateWord<F>> for CellValue<F> {
    fn from(state_word: StateWord<F>) -> CellValue<F> {
        CellValue::new(state_word.var, state_word.value)
    }
}

impl<F: FieldExt> From<CellValue<F>> for StateWord<F> {
    fn from(cell_value: CellValue<F>) -> StateWord<F> {
        StateWord::new(cell_value.cell(), cell_value.value())
    }
}

#[derive(Debug)]
struct Pow5State<F: FieldExt, const WIDTH: usize>([StateWord<F>; WIDTH]);

impl<F: FieldExt, const WIDTH: usize> Pow5State<F, WIDTH> {
    fn full_round<const RATE: usize>(
        self,
        region: &mut Region<F>,
        config: &Pow5Config<F, WIDTH, RATE>,
        round: usize,
        offset: usize,
    ) -> Result<Self, Error> {
        Self::round(region, config, round, offset, config.s_full, |_| {
            let q = self
                .0
                .iter()
                .enumerate()
                .map(|(idx, word)| word.value.map(|v| v + config.round_constants[round][idx]));
            let r: Option<Vec<F>> = q.map(|q| q.map(|q| q.pow(&config.alpha))).collect();
            let m = &config.m_reg;
            let state = m.iter().map(|m_i| {
                r.as_ref().map(|r| {
                    r.iter()
                        .enumerate()
                        .fold(F::zero(), |acc, (j, r_j)| acc + m_i[j] * r_j)
                })
            });

            Ok((round + 1, state.collect::<Vec<_>>().try_into().unwrap()))
        })
    }

    fn partial_round<const RATE: usize>(
        self,
        region: &mut Region<F>,
        config: &Pow5Config<F, WIDTH, RATE>,
        round: usize,
        offset: usize,
    ) -> Result<Self, Error> {
        Self::round(region, config, round, offset, config.s_partial, |region| {
            let m = &config.m_reg;
            let p: Option<Vec<_>> = self.0.iter().map(|word| word.value).collect();

            let r: Option<Vec<_>> = p.map(|p| {
                let r_0 = (p[0] + config.round_constants[round][0]).pow(&config.alpha);
                let r_i = p[1..]
                    .iter()
                    .enumerate()
                    .map(|(i, p_i)| *p_i + config.round_constants[round][i + 1]);
                std::iter::empty().chain(Some(r_0)).chain(r_i).collect()
            });

            region.assign_advice(
                || format!("round_{} partial_sbox", round),
                config.partial_sbox,
                offset,
                || r.as_ref().map(|r| r[0]).ok_or(Error::SynthesisError),
            )?;

            let p_mid: Option<Vec<_>> = m
                .iter()
                .map(|m_i| {
                    r.as_ref().map(|r| {
                        r.iter()
                            .enumerate()
                            .fold(F::zero(), |acc, (j, r_j)| acc + m_i[j] * r_j)
                    })
                })
                .collect();

            // Load the second round constants.
            let mut load_round_constant = |i: usize| {
                region.assign_fixed(
                    || format!("round_{} rc_{}", round + 1, i),
                    config.rc_b[i],
                    offset,
                    || Ok(config.round_constants[round + 1][i]),
                )
            };
            for i in 0..WIDTH {
                load_round_constant(i)?;
            }

            let r_mid: Option<Vec<_>> = p_mid.map(|p| {
                let r_0 = (p[0] + config.round_constants[round + 1][0]).pow(&config.alpha);
                let r_i = p[1..]
                    .iter()
                    .enumerate()
                    .map(|(i, p_i)| *p_i + config.round_constants[round + 1][i + 1]);
                std::iter::empty().chain(Some(r_0)).chain(r_i).collect()
            });

            let state: Vec<Option<_>> = m
                .iter()
                .map(|m_i| {
                    r_mid.as_ref().map(|r| {
                        r.iter()
                            .enumerate()
                            .fold(F::zero(), |acc, (j, r_j)| acc + m_i[j] * r_j)
                    })
                })
                .collect();

            Ok((round + 2, state.try_into().unwrap()))
        })
    }

    fn load<const RATE: usize>(
        region: &mut Region<F>,
        config: &Pow5Config<F, WIDTH, RATE>,
        initial_state: &State<StateWord<F>, WIDTH>,
    ) -> Result<Self, Error> {
        let load_state_word = |i: usize| {
            let value = initial_state[i].value;
            let var = region.assign_advice(
                || format!("load state_{}", i),
                config.state[i],
                0,
                || value.ok_or(Error::SynthesisError),
            )?;
            region.constrain_equal(initial_state[i].var, var)?;
            Ok(StateWord { var, value })
        };

        let state: Result<Vec<_>, _> = (0..WIDTH).map(load_state_word).collect();
        state.map(|state| Pow5State(state.try_into().unwrap()))
    }

    fn round<const RATE: usize>(
        region: &mut Region<F>,
        config: &Pow5Config<F, WIDTH, RATE>,
        round: usize,
        offset: usize,
        round_gate: Selector,
        round_fn: impl FnOnce(&mut Region<F>) -> Result<(usize, [Option<F>; WIDTH]), Error>,
    ) -> Result<Self, Error> {
        // Enable the required gate.
        round_gate.enable(region, offset)?;

        // Load the round constants.
        let mut load_round_constant = |i: usize| {
            region.assign_fixed(
                || format!("round_{} rc_{}", round, i),
                config.rc_a[i],
                offset,
                || Ok(config.round_constants[round][i]),
            )
        };
        for i in 0..WIDTH {
            load_round_constant(i)?;
        }

        // Compute the next round's state.
        let (next_round, next_state) = round_fn(region)?;

        let next_state_word = |i: usize| {
            let value = next_state[i];
            let var = region.assign_advice(
                || format!("round_{} state_{}", next_round, i),
                config.state[i],
                offset + 1,
                || value.ok_or(Error::SynthesisError),
            )?;
            Ok(StateWord { var, value })
        };

        let next_state: Result<Vec<_>, _> = (0..WIDTH).map(next_state_word).collect();
        next_state.map(|next_state| Pow5State(next_state.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use ff::PrimeField;
    use halo2::{
        arithmetic::FieldExt,
        circuit::{Layouter, SimpleFloorPlanner},
        dev::MockProver,
        pasta::Fp,
        plonk::{Circuit, ConstraintSystem, Error},
    };
    use pasta_curves::pallas;

    use super::{PoseidonInstructions, Pow5Chip, Pow5Config, StateWord};
    use crate::{
        circuit::gadget::{
            poseidon::Hash,
            utilities::{CellValue, Var},
        },
        primitives::poseidon::{self, ConstantLength, P128Pow5T3 as OrchardNullifier, Spec},
    };
    use std::convert::TryInto;
    use std::marker::PhantomData;

    struct PermuteCircuit<S: Spec<Fp, WIDTH, RATE>, const WIDTH: usize, const RATE: usize>(
        PhantomData<S>,
    );

    impl<S: Spec<Fp, WIDTH, RATE>, const WIDTH: usize, const RATE: usize> Circuit<Fp>
        for PermuteCircuit<S, WIDTH, RATE>
    {
        type Config = Pow5Config<Fp, WIDTH, RATE>;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            PermuteCircuit::<S, WIDTH, RATE>(PhantomData)
        }

        fn configure(meta: &mut ConstraintSystem<Fp>) -> Pow5Config<Fp, WIDTH, RATE> {
            let state = (0..WIDTH).map(|_| meta.advice_column()).collect::<Vec<_>>();
            let partial_sbox = meta.advice_column();

            let rc_a = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();
            let rc_b = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();

            Pow5Chip::configure::<S>(
                meta,
                state.try_into().unwrap(),
                partial_sbox,
                rc_a.try_into().unwrap(),
                rc_b.try_into().unwrap(),
            )
        }

        fn synthesize(
            &self,
            config: Pow5Config<Fp, WIDTH, RATE>,
            mut layouter: impl Layouter<Fp>,
        ) -> Result<(), Error> {
            let initial_state = layouter.assign_region(
                || "prepare initial state",
                |mut region| {
                    let state_word = |i: usize| {
                        let value = Some(Fp::from(i as u64));
                        let var = region.assign_advice(
                            || format!("load state_{}", i),
                            config.state[i],
                            0,
                            || value.ok_or(Error::SynthesisError),
                        )?;
                        Ok(StateWord { var, value })
                    };

                    let state: Result<Vec<_>, Error> = (0..WIDTH).map(state_word).collect();
                    Ok(state?.try_into().unwrap())
                },
            )?;

            let chip = Pow5Chip::construct(config.clone());
            let final_state = <Pow5Chip<_, WIDTH, RATE> as PoseidonInstructions<
                Fp,
                S,
                WIDTH,
                RATE,
            >>::permute(&chip, &mut layouter, &initial_state)?;

            // For the purpose of this test, compute the real final state inline.
            let mut expected_final_state = (0..WIDTH)
                .map(|idx| Fp::from_u64(idx as u64))
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            let (round_constants, mds, _) = S::constants();
            poseidon::permute::<_, S, WIDTH, RATE>(
                &mut expected_final_state,
                &mds,
                &round_constants,
            );

            layouter.assign_region(
                || "constrain final state",
                |mut region| {
                    let mut final_state_word = |i: usize| {
                        let var = region.assign_advice(
                            || format!("load final_state_{}", i),
                            config.state[i],
                            0,
                            || Ok(expected_final_state[i]),
                        )?;
                        region.constrain_equal(final_state[i].var, var)
                    };

                    for i in 0..(WIDTH - 1) {
                        final_state_word(i)?;
                    }

                    final_state_word(WIDTH - 1)
                },
            )
        }
    }

    #[test]
    fn poseidon_permute() {
        let k = 6;
        let circuit = PermuteCircuit::<OrchardNullifier, 3, 2>(PhantomData);
        let prover = MockProver::run(k, &circuit, vec![]).unwrap();
        assert_eq!(prover.verify(), Ok(()))
    }

    struct HashCircuit<S: Spec<Fp, WIDTH, RATE>, const WIDTH: usize, const RATE: usize> {
        message: Option<[Fp; 2]>,
        // For the purpose of this test, witness the result.
        // TODO: Move this into an instance column.
        output: Option<Fp>,
        _spec: PhantomData<S>,
    }

    impl<S: Spec<Fp, WIDTH, RATE>, const WIDTH: usize, const RATE: usize> Circuit<Fp>
        for HashCircuit<S, WIDTH, RATE>
    {
        type Config = Pow5Config<Fp, WIDTH, RATE>;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self {
                message: None,
                output: None,
                _spec: PhantomData,
            }
        }

        fn configure(meta: &mut ConstraintSystem<Fp>) -> Pow5Config<Fp, WIDTH, RATE> {
            let state = (0..WIDTH).map(|_| meta.advice_column()).collect::<Vec<_>>();
            let partial_sbox = meta.advice_column();

            let rc_a = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();
            let rc_b = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();

            meta.enable_constant(rc_b[0]);

            Pow5Chip::configure::<S>(
                meta,
                state.try_into().unwrap(),
                partial_sbox,
                rc_a.try_into().unwrap(),
                rc_b.try_into().unwrap(),
            )
        }

        fn synthesize(
            &self,
            config: Pow5Config<Fp, WIDTH, RATE>,
            mut layouter: impl Layouter<Fp>,
        ) -> Result<(), Error> {
            let chip = Pow5Chip::construct(config.clone());

            let message = layouter.assign_region(
                || "load message",
                |mut region| {
                    let message_word = |i: usize| {
                        let value = self.message.map(|message_vals| message_vals[i]);
                        let cell = region.assign_advice(
                            || format!("load message_{}", i),
                            config.state[i],
                            0,
                            || value.ok_or(Error::SynthesisError),
                        )?;
                        Ok(CellValue::new(cell, value))
                    };

                    let message: Result<Vec<_>, Error> = (0..RATE).map(message_word).collect();
                    Ok(message?.try_into().unwrap())
                },
            )?;

            let hasher = Hash::<_, _, S, _, WIDTH, RATE>::init(
                chip,
                layouter.namespace(|| "init"),
                ConstantLength::<RATE>,
            )?;
            let output = hasher.hash(layouter.namespace(|| "hash"), message)?;

            layouter.assign_region(
                || "constrain output",
                |mut region| {
                    let expected_var = region.assign_advice(
                        || "load output",
                        config.state[0],
                        0,
                        || self.output.ok_or(Error::SynthesisError),
                    )?;
                    region.constrain_equal(output.cell(), expected_var)
                },
            )
        }
    }

    #[test]
    fn poseidon_hash() {
        let message = [Fp::rand(), Fp::rand()];
        let output =
            poseidon::Hash::<_, OrchardNullifier, _, 3, 2>::init(ConstantLength::<2>).hash(message);

        let k = 6;
        let circuit = HashCircuit::<OrchardNullifier, 3, 2> {
            message: Some(message),
            output: Some(output),
            _spec: PhantomData,
        };
        let prover = MockProver::run(k, &circuit, vec![]).unwrap();
        assert_eq!(prover.verify(), Ok(()))
    }

    #[test]
    fn hash_test_vectors() {
        for tv in crate::primitives::poseidon::test_vectors::fp::hash() {
            let message = [
                pallas::Base::from_repr(tv.input[0]).unwrap(),
                pallas::Base::from_repr(tv.input[1]).unwrap(),
            ];
            let output =
                poseidon::Hash::<_, OrchardNullifier, _, 3, 2>::init(ConstantLength).hash(message);

            let k = 6;
            let circuit = HashCircuit::<OrchardNullifier, 3, 2> {
                message: Some(message),
                output: Some(output),
                _spec: PhantomData,
            };
            let prover = MockProver::run(k, &circuit, vec![]).unwrap();
            assert_eq!(prover.verify(), Ok(()));
        }
    }

    #[cfg(feature = "dev-graph")]
    #[test]
    fn print_poseidon_chip() {
        use plotters::prelude::*;

        let root = BitMapBackend::new("poseidon-chip-layout.png", (1024, 768)).into_drawing_area();
        root.fill(&WHITE).unwrap();
        let root = root
            .titled("Poseidon Chip Layout", ("sans-serif", 60))
            .unwrap();

        let circuit = HashCircuit::<OrchardNullifier, 3, 2> {
            message: None,
            output: None,
            _spec: PhantomData,
        };
        halo2::dev::CircuitLayout::default()
            .render(6, &circuit, &root)
            .unwrap();
    }
}
