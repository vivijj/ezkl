use ezkl::circuit::region::RegionCtx;
use ezkl::circuit::{
    ops::lookup::LookupOp, ops::poly::PolyOp, BaseConfig as PolyConfig, CheckMode,
};
use ezkl::fieldutils::i32_to_felt;
use ezkl::tensor::*;
use halo2_proofs::dev::MockProver;
use halo2_proofs::{
    circuit::{Layouter, SimpleFloorPlanner, Value},
    plonk::{Circuit, Column, ConstraintSystem, Error, Instance},
};
use halo2curves::bn256::Fr as F;
use std::marker::PhantomData;

const K: usize = 15;
// A columnar ReLu MLP
#[derive(Clone)]
struct MyConfig {
    layer_config: PolyConfig<F>,
    public_output: Column<Instance>,
}

#[derive(Clone)]
struct MyCircuit<
    const LEN: usize, //LEN = CHOUT x OH x OW flattened
    const BITS: usize,
> {
    // Given the stateless MyConfig type information, a DNN trace is determined by its input and the parameters of its layers.
    // Computing the trace still requires a forward pass. The intermediate activations are stored only by the layouter.
    input: ValTensor<F>,
    l0_params: [Tensor<F>; 2],
    l2_params: [Tensor<F>; 2],
    _marker: PhantomData<F>,
}

impl<const LEN: usize, const BITS: usize> Circuit<F> for MyCircuit<LEN, BITS> {
    type Config = MyConfig;
    type FloorPlanner = SimpleFloorPlanner;
    type Params = PhantomData<F>;

    fn without_witnesses(&self) -> Self {
        self.clone()
    }

    // Here we wire together the layers by using the output advice in each layer as input advice in the next (not with copying / equality).
    // This can be automated but we will sometimes want skip connections, etc. so we need the flexibility.
    fn configure(cs: &mut ConstraintSystem<F>) -> Self::Config {
        let input = VarTensor::new_advice(cs, K, LEN);
        let params = VarTensor::new_advice(cs, K, LEN * LEN);
        let output = VarTensor::new_advice(cs, K, LEN);
        // tells the config layer to add an affine op to the circuit gate

        let mut layer_config =
            PolyConfig::<F>::configure(cs, &[input.clone(), params], &output, CheckMode::SAFE);

        // sets up a new ReLU table and resuses it for l1 and l3 non linearities
        layer_config
            .configure_lookup(cs, &input, &output, BITS, &LookupOp::ReLU { scale: 1 })
            .unwrap();

        // sets up a new ReLU table and resuses it for l1 and l3 non linearities
        layer_config
            .configure_lookup(
                cs,
                &input,
                &output,
                BITS,
                &LookupOp::Div {
                    denom: ezkl::circuit::utils::F32::from(128.),
                },
            )
            .unwrap();

        let public_output: Column<Instance> = cs.instance_column();
        cs.enable_equality(public_output);

        MyConfig {
            layer_config,
            public_output,
        }
    }

    fn synthesize(
        &self,
        mut config: Self::Config,
        mut layouter: impl Layouter<F>,
    ) -> Result<(), Error> {
        config.layer_config.layout_tables(&mut layouter).unwrap();

        let x = layouter
            .assign_region(
                || "mlp_4d",
                |region| {
                    let mut region = RegionCtx::new(region, 0);
                    let x = config
                        .layer_config
                        .layout(
                            &mut region,
                            &[self.l0_params[0].clone().into(), self.input.clone()],
                            Box::new(PolyOp::Einsum {
                                equation: "ab,bc->ac".to_string(),
                            }),
                        )
                        .unwrap()
                        .unwrap();
                    println!("1");
                    println!("offset: {}", region.offset());
                    println!("x shape: {:?}", x.dims());

                    let x = config
                        .layer_config
                        .layout(
                            &mut region,
                            &[x],
                            Box::new(PolyOp::Add {
                                a: Some(self.l0_params[1].clone()),
                            }),
                        )
                        .unwrap()
                        .unwrap();
                    println!("2");
                    println!("offset: {}", region.offset());
                    println!("x shape: {:?}", x.dims());
                    let mut x = config
                        .layer_config
                        .layout(&mut region, &[x], Box::new(LookupOp::ReLU { scale: 1 }))
                        .unwrap()
                        .unwrap();
                    println!("3");
                    println!("offset: {}", region.offset());
                    println!("x shape: {:?}", x.dims());
                    x.reshape(&[x.dims()[0], 1]).unwrap();
                    let x = config
                        .layer_config
                        .layout(
                            &mut region,
                            &[self.l2_params[0].clone().into(), x],
                            Box::new(PolyOp::Einsum {
                                equation: "ab,bc->ac".to_string(),
                            }),
                        )
                        .unwrap()
                        .unwrap();
                    println!("4");
                    println!("offset: {}", region.offset());
                    println!("x shape: {:?}", x.dims());

                    let x = config
                        .layer_config
                        .layout(
                            &mut region,
                            &[x],
                            Box::new(PolyOp::Add {
                                a: Some(self.l2_params[1].clone()),
                            }),
                        )
                        .unwrap()
                        .unwrap();
                    println!("5");
                    println!("offset: {}", region.offset());
                    println!("x shape: {:?}", x.dims());
                    let x = config
                        .layer_config
                        .layout(&mut region, &[x], Box::new(LookupOp::ReLU { scale: 1 }))
                        .unwrap();
                    println!("6");
                    println!("offset: {}", region.offset());
                    Ok(config
                        .layer_config
                        .layout(
                            &mut region,
                            &[x.unwrap()],
                            Box::new(LookupOp::Div {
                                denom: ezkl::circuit::utils::F32::from(128.),
                            }),
                        )
                        .unwrap())
                },
            )
            .unwrap();
        match x.unwrap() {
            ValTensor::Value {
                inner: v, dims: _, ..
            } => v
                .enum_map(|i, x| match x {
                    ValType::PrevAssigned(v) => {
                        layouter.constrain_instance(v.cell(), config.public_output, i)
                    }
                    _ => panic!(),
                })
                .unwrap(),
            _ => panic!("Should be assigned"),
        };
        Ok(())
    }
}

pub fn runmlp() {
    env_logger::init();
    // parameters
    let mut l0_kernel: Tensor<F> = Tensor::<i32>::new(
        Some(&[10, 0, 0, -1, 0, 10, 1, 0, 0, 1, 10, 0, 1, 0, 0, 10]),
        &[4, 4],
    )
    .unwrap()
    .map(i32_to_felt);
    l0_kernel.set_visibility(ezkl::graph::Visibility::Private);

    let mut l0_bias: Tensor<F> = Tensor::<i32>::new(Some(&[0, 0, 0, 1]), &[4, 1])
        .unwrap()
        .map(i32_to_felt);
    l0_bias.set_visibility(ezkl::graph::Visibility::Private);

    let mut l2_kernel: Tensor<F> = Tensor::<i32>::new(
        Some(&[0, 3, 10, -1, 0, 10, 1, 0, 0, 1, 0, 12, 1, -2, 32, 0]),
        &[4, 4],
    )
    .unwrap()
    .map(i32_to_felt);
    l2_kernel.set_visibility(ezkl::graph::Visibility::Private);
    // input data, with 1 padding to allow for bias
    let input: Tensor<Value<F>> = Tensor::<i32>::new(Some(&[-30, -21, 11, 40]), &[4, 1])
        .unwrap()
        .into();
    let mut l2_bias: Tensor<F> = Tensor::<i32>::new(Some(&[0, 0, 0, 1]), &[4, 1])
        .unwrap()
        .map(i32_to_felt);
    l2_bias.set_visibility(ezkl::graph::Visibility::Private);

    let circuit = MyCircuit::<4, 14> {
        input: input.into(),
        l0_params: [l0_kernel, l0_bias],
        l2_params: [l2_kernel, l2_bias],
        _marker: PhantomData,
    };

    let public_input: Vec<i32> = unsafe {
        vec![
            (531f32 / 128f32).round().to_int_unchecked::<i32>(),
            (103f32 / 128f32).round().to_int_unchecked::<i32>(),
            (4469f32 / 128f32).round().to_int_unchecked::<i32>(),
            (2849f32 / 128f32).to_int_unchecked::<i32>(),
        ]
    };

    println!("public input {:?}", public_input);

    let prover = MockProver::run(
        K as u32,
        &circuit,
        vec![public_input.iter().map(|x| i32_to_felt::<F>(*x)).collect()],
    )
    .unwrap();
    prover.assert_satisfied();
}

pub fn main() {
    runmlp()
}
