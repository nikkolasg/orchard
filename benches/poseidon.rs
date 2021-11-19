use ff::Field;
use halo2::{
    circuit::{Layouter, SimpleFloorPlanner},
    pasta::Fp,
    plonk::{
        create_proof, keygen_pk, keygen_vk, verify_proof, Advice, Circuit, Column,
        ConstraintSystem, Error,
    },
    poly::commitment::Params,
    transcript::{Blake2bRead, Blake2bWrite, Challenge255},
};
use pasta_curves::{pallas, vesta};

use orchard::{
    circuit::gadget::{
        poseidon::{Hash, Pow5Chip, Pow5Config},
        utilities::{CellValue, Var},
    },
    primitives::poseidon::{self, ConstantLength, Spec},
};
use std::convert::TryInto;
use std::marker::PhantomData;

use criterion::{criterion_group, criterion_main, Criterion};
use rand::rngs::OsRng;

#[derive(Clone, Copy)]
struct HashCircuit<S, const WIDTH: usize, const RATE: usize>
where
    S: Spec<Fp, WIDTH, RATE> + Clone + Copy,
{
    message: Option<[Fp; RATE]>,
    // For the purpose of this test, witness the result.
    // TODO: Move this into an instance column.
    output: Option<Fp>,
    _spec: PhantomData<S>,
}

#[derive(Debug, Clone)]
struct MyConfig<const WIDTH: usize, const RATE: usize> {
    input: [Column<Advice>; RATE],
    poseidon_config: Pow5Config<Fp, WIDTH, RATE>,
}

impl<S, const WIDTH: usize, const RATE: usize> Circuit<Fp> for HashCircuit<S, WIDTH, RATE>
where
    S: Spec<Fp, WIDTH, RATE> + Copy + Clone,
{
    type Config = MyConfig<WIDTH, RATE>;
    type FloorPlanner = SimpleFloorPlanner;

    fn without_witnesses(&self) -> Self {
        Self {
            message: None,
            output: None,
            _spec: PhantomData,
        }
    }

    fn configure(meta: &mut ConstraintSystem<Fp>) -> Self::Config {
        let state = (0..WIDTH).map(|_| meta.advice_column()).collect::<Vec<_>>();
        let partial_sbox = meta.advice_column();

        let rc_a = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();
        let rc_b = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();

        meta.enable_constant(rc_b[0]);

        Self::Config {
            input: state[..RATE].try_into().unwrap(),
            poseidon_config: Pow5Chip::configure::<S>(
                meta,
                state.try_into().unwrap(),
                partial_sbox,
                rc_a.try_into().unwrap(),
                rc_b.try_into().unwrap(),
            ),
        }
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<Fp>,
    ) -> Result<(), Error> {
        let chip = Pow5Chip::construct(config.poseidon_config.clone());

        let message = layouter.assign_region(
            || "load message",
            |mut region| {
                let message_word = |i: usize| {
                    let value = self.message.map(|message_vals| message_vals[i]);
                    let cell = region.assign_advice(
                        || format!("load message_{}", i),
                        config.input[i],
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
                    config.input[0],
                    0,
                    || self.output.ok_or(Error::SynthesisError),
                )?;
                region.constrain_equal(output.cell(), expected_var)
            },
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct MySpec<const WIDTH: usize, const RATE: usize>;

impl Spec<Fp, 3, 2> for MySpec<3, 2> {
    fn full_rounds() -> usize {
        8
    }

    fn partial_rounds() -> usize {
        56
    }

    fn sbox(val: Fp) -> Fp {
        val.pow_vartime(&[5])
    }

    fn secure_mds() -> usize {
        0
    }
}

impl Spec<Fp, 9, 8> for MySpec<9, 8> {
    fn full_rounds() -> usize {
        8
    }

    fn partial_rounds() -> usize {
        56
    }

    fn sbox(val: Fp) -> Fp {
        val.pow_vartime(&[5])
    }

    fn secure_mds() -> usize {
        0
    }
}

impl Spec<Fp, 12, 11> for MySpec<12, 11> {
    fn full_rounds() -> usize {
        8
    }

    fn partial_rounds() -> usize {
        56
    }

    fn sbox(val: Fp) -> Fp {
        val.pow_vartime(&[5])
    }

    fn secure_mds() -> usize {
        0
    }
}

const K: u32 = 6;

fn bench_poseidon<S, const WIDTH: usize, const RATE: usize>(name: &str, c: &mut Criterion)
where
    S: Spec<Fp, WIDTH, RATE> + Copy + Clone,
{
    // Initialize the polynomial commitment parameters
    let params: Params<vesta::Affine> = Params::new(K);

    let empty_circuit = HashCircuit::<S, WIDTH, RATE> {
        message: None,
        output: None,
        _spec: PhantomData,
    };

    // Initialize the proving key
    let vk = keygen_vk(&params, &empty_circuit).expect("keygen_vk should not fail");
    let pk = keygen_pk(&params, vk, &empty_circuit).expect("keygen_pk should not fail");

    let prover_name = name.to_string() + "-prover";
    let verifier_name = name.to_string() + "-verifier";

    let rng = OsRng;
    let message = (0..RATE)
        .map(|_| pallas::Base::random(rng))
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let output = poseidon::Hash::<_, S, _, WIDTH, RATE>::init(ConstantLength::<RATE>).hash(message);

    let circuit = HashCircuit::<S, WIDTH, RATE> {
        message: Some(message),
        output: Some(output),
        _spec: PhantomData,
    };

    c.bench_function(&prover_name, |b| {
        b.iter(|| {
            // Create a proof
            let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
            create_proof(&params, &pk, &[circuit], &[&[]], &mut transcript)
                .expect("proof generation should not fail")
        })
    });

    // Create a proof
    let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
    create_proof(&params, &pk, &[circuit], &[&[]], &mut transcript)
        .expect("proof generation should not fail");
    let proof = transcript.finalize();

    c.bench_function(&verifier_name, |b| {
        b.iter(|| {
            let msm = params.empty_msm();
            let mut transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof[..]);
            let guard = verify_proof(&params, pk.get_vk(), msm, &[&[]], &mut transcript).unwrap();
            let msm = guard.clone().use_challenges();
            assert!(msm.eval());
        });
    });
}

fn criterion_benchmark(c: &mut Criterion) {
    bench_poseidon::<MySpec<3, 2>, 3, 2>("WIDTH = 3, RATE = 2", c);
    bench_poseidon::<MySpec<9, 8>, 9, 8>("WIDTH = 9, RATE = 8", c);
    bench_poseidon::<MySpec<12, 11>, 12, 11>("WIDTH = 12, RATE = 11", c);
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
